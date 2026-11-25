use anyhow::{Context, Result, ensure};
use std::{
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::Command,
    sync::{broadcast, mpsc, watch},
};

pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const MAX_AGE: Duration = Duration::from_millis(200);
pub const QUEUE: usize = 4;

#[derive(Clone)]
pub struct Frame {
    pub at: Instant,
    pub ticks: u64,
    pub data: Arc<[u8]>,
}

impl Frame {
    pub fn key(&self) -> bool {
        self.dimensions().is_some()
    }

    pub fn valid(&self) -> bool {
        self.data.first().is_some_and(|tag| tag & 1 != 0) || self.key()
    }

    fn dimensions(&self) -> Option<(u16, u16)> {
        if self.data.len() < 10 || self.data[0] & 1 != 0 || self.data[3..6] != [0x9d, 0x01, 0x2a] {
            return None;
        }
        let width = u16::from_le_bytes(self.data[6..8].try_into().ok()?) & 0x3fff;
        let height = u16::from_le_bytes(self.data[8..10].try_into().ok()?) & 0x3fff;
        ((1..=1920).contains(&width) && (1..=1080).contains(&height)).then_some((width, height))
    }
}

pub struct Video {
    pub sharing: watch::Sender<bool>,
    pub viewers: watch::Sender<usize>,
    pub frames: broadcast::Sender<Frame>,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            sharing: watch::Sender::new(false),
            viewers: watch::Sender::new(0),
            frames: broadcast::Sender::new(QUEUE),
        }
    }
}

impl Video {
    pub fn stop(&self) {
        self.sharing.send_replace(false);
    }
}

fn ivf(width: u16, height: u16) -> [u8; 32] {
    let mut header = [0; 32];
    header[..4].copy_from_slice(b"DKIF");
    header[6] = 32;
    header[8..12].copy_from_slice(b"VP80");
    header[12..14].copy_from_slice(&width.to_le_bytes());
    header[14..16].copy_from_slice(&height.to_le_bytes());
    header[16..20].copy_from_slice(&90_000u32.to_le_bytes());
    header[20] = 1;
    header
}

pub async fn play(
    frames: mpsc::Receiver<Frame>,
    mut stop: watch::Receiver<bool>,
    request_keyframe: impl Fn(),
) -> Result<()> {
    let mut command = Command::new("mpv");
    command.arg("--vo=tct");
    let mut child = command
        .args([
            "--no-config",
            "--profile=low-latency",
            "--untimed", // The sender already paces this live stream; don't schedule it again.
            "--no-audio",
            "--really-quiet",
            "--input-terminal=no",
            "--demuxer=lavf",
            "--demuxer-lavf-format=ivf",
        ])
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Could not start mpv")?;
    let mut pipe = BufWriter::new(child.stdin.take().unwrap());
    let result = tokio::select! {
        _ = stop.wait_for(|stop| *stop) => Ok(()),
        result = write_ivf(frames, &mut pipe, request_keyframe) => result,
        status = child.wait() => {
            ensure!(status?.success(), "mpv could not play this stream");
            return Ok(());
        }
    };
    drop(pipe);
    if tokio::time::timeout(Duration::from_millis(500), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    result
}

async fn write_ivf(
    mut frames: mpsc::Receiver<Frame>,
    mut pipe: impl tokio::io::AsyncWrite + Unpin,
    request_keyframe: impl Fn(),
) -> Result<()> {
    let mut base = None;
    let mut need_key = true;
    while let Some(frame) = frames.recv().await {
        if frame.at.elapsed() > MAX_AGE {
            if !need_key {
                request_keyframe();
            }
            need_key = true;
            continue;
        }
        if need_key && !frame.key() {
            continue;
        }
        need_key = false;
        // Finish a whole frame even if it spends longer than MAX_AGE in the pipe.
        tokio::time::timeout(Duration::from_secs(5), async {
            if base.is_none()
                && let Some((width, height)) = frame.dimensions()
            {
                pipe.write_all(&ivf(width, height)).await?;
                base = Some(frame.ticks);
            }
            let mut header = [0; 12];
            header[..4].copy_from_slice(&(frame.data.len() as u32).to_le_bytes());
            header[4..].copy_from_slice(&frame.ticks.saturating_sub(base.unwrap()).to_le_bytes());
            pipe.write_all(&header).await?;
            pipe.write_all(&frame.data).await?;
            pipe.flush().await
        })
        .await
        .context("mpv stopped reading the video pipe")??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_player_finishes_its_frame_and_skips_stale_deltas() -> Result<()> {
        use tokio::io::AsyncReadExt;
        let (tx, frames) = mpsc::channel(QUEUE);
        let (pipe, mut player) = tokio::io::duplex(1);
        let (key, mut requested) = watch::channel(false);
        let writing = tokio::spawn(write_ivf(frames, pipe, move || {
            key.send_replace(true);
        }));
        let data: Arc<[u8]> = Arc::from([0, 0, 0, 0x9d, 1, 0x2a, 0x80, 7, 0x38, 4]);
        tx.send(Frame {
            at: Instant::now(),
            ticks: 0,
            data: data.clone(),
        })
        .await?;
        let mut first = [0; 54];
        player.read_exact(&mut first[..1]).await?;
        tx.send(Frame {
            at: Instant::now(),
            ticks: 1,
            data: Arc::from([1]),
        })
        .await?;
        tokio::time::sleep(MAX_AGE + Duration::from_millis(25)).await;
        player.read_exact(&mut first[1..]).await?;
        assert_eq!(&first[..4], b"DKIF");
        assert_eq!(&first[44..], &*data);
        tokio::time::timeout(Duration::from_secs(1), requested.wait_for(|key| *key)).await??;
        tx.send(Frame {
            at: Instant::now(),
            ticks: 2,
            data,
        })
        .await?;
        drop(tx);
        let mut second = Vec::new();
        player.read_to_end(&mut second).await?;
        assert_eq!(second.len(), 22);
        assert_eq!(u64::from_le_bytes(second[4..12].try_into()?), 2);
        writing.await??;
        Ok(())
    }

    #[test]
    fn ivf_uses_keyframe_dimensions_and_rtp_clock() {
        let frame = Frame {
            at: Instant::now(),
            ticks: 0,
            data: Arc::from([0, 0, 0, 0x9d, 1, 0x2a, 0x80, 0x07, 0x38, 0x04]),
        };
        assert_eq!(frame.dimensions(), Some((1920, 1080)));
        assert_eq!(
            &ivf(1920, 1080)[8..24],
            b"VP80\x80\x07\x38\x04\x90\x5f\x01\x00\x01\x00\x00\x00"
        );
    }
}
