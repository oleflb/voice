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

#[cfg(target_os = "linux")]
pub(crate) const VP8_PIPELINE: &[&str] = &[
    "!",
    "videorate",
    "max-rate=30",
    "!",
    "videoconvertscale",
    "add-borders=true",
    "!",
    "video/x-raw,format=I420,width=1920,height=1080,pixel-aspect-ratio=1/1",
    "!",
    "vp8enc",
    "deadline=1",
    "cpu-used=8",
    "threads=4",
    "lag-in-frames=0",
    "target-bitrate=5000000",
    "keyframe-max-dist=15",
    "!",
    "gdppay",
    "!",
    "fdsink",
    "fd=1",
    "sync=false",
];

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
    #[cfg(target_os = "linux")]
    capture: parking_lot::Mutex<Option<watch::Sender<bool>>>,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            sharing: watch::Sender::new(false),
            viewers: watch::Sender::new(0),
            frames: broadcast::Sender::new(QUEUE),
            #[cfg(target_os = "linux")]
            capture: Default::default(),
        }
    }
}

impl Video {
    pub fn stop(&self) {
        #[cfg(target_os = "linux")]
        let capture = self.capture.lock();
        self.sharing.send_replace(false);
        #[cfg(target_os = "linux")]
        if let Some(stop) = capture.as_ref() {
            stop.send_replace(true);
        }
    }

    #[cfg(target_os = "linux")]
    fn announce_share(&self) -> bool {
        let capture = self.capture.lock();
        if capture.as_ref().is_none_or(|stop| *stop.borrow()) {
            return false;
        }
        self.sharing.send_replace(true);
        true
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalVideo {
    Blocks,
    Kitty,
    KittyShm,
}

pub fn probe_kitty_graphics() -> TerminalVideo {
    #[cfg(unix)]
    {
        use ratatui_image::picker::cap_parser::Parser;
        use std::io::{self, IsTerminal, Write};
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        if stdin.is_terminal() && stdout.is_terminal() {
            let tmux = std::env::var_os("TMUX").is_some();
            let query = Parser::query(tmux, Default::default());
            if stdout
                .write_all(query.as_bytes())
                .and_then(|()| stdout.flush())
                .is_ok()
                && read_kitty_reply(&stdin, Duration::from_millis(300)).unwrap_or(false)
            {
                return if probe_kitty_shm(&stdin, &mut stdout, tmux).unwrap_or(false) {
                    TerminalVideo::KittyShm
                } else {
                    TerminalVideo::Kitty
                };
            }
        }
    }
    TerminalVideo::Blocks
}

#[cfg(unix)]
fn probe_kitty_shm(
    input: impl std::os::fd::AsFd,
    output: &mut impl std::io::Write,
    tmux: bool,
) -> Result<bool> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ratatui_image::picker::cap_parser::Parser;
    use rustix::{fs::Mode, shm};
    use std::io::Write;

    let name = format!("/voice-probe-{}", std::process::id());
    let flags = shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR;
    let fd = shm::open(name.as_str(), flags, Mode::RUSR | Mode::WUSR)?;
    let result = (|| {
        std::fs::File::from(fd).write_all(&[0; 3])?;
        let (start, esc, end) = Parser::tmux_start_escape_end(tmux);
        let payload = STANDARD.encode(&name);
        write!(
            output,
            "{start}{esc}_Gi=31,s=1,v=1,a=q,t=s,f=24;{payload}{esc}\\{esc}[5n{end}"
        )?;
        output.flush()?;
        Ok(read_kitty_reply(input, Duration::from_millis(300))?)
    })();
    let _ = shm::unlink(name.as_str());
    result
}

#[cfg(unix)]
fn read_kitty_reply(input: impl std::os::fd::AsFd, timeout: Duration) -> rustix::io::Result<bool> {
    use ratatui_image::picker::cap_parser::{Parser, Response};
    use rustix::event::{PollFd, PollFlags, poll};

    let mut fds = [PollFd::new(&input, PollFlags::IN)];
    let mut parser = Parser::new();
    let started = Instant::now();
    let mut kitty = false;
    // Keep stdin on this thread: a detached blocking read could steal the next keypress.
    while let Some(remaining) = timeout.checked_sub(started.elapsed()) {
        if poll(
            &mut fds,
            Some(&remaining.try_into().expect("short probe timeout")),
        )? == 0
        {
            break;
        }
        let mut bytes = [0; 128];
        let read = rustix::io::read(&input, &mut bytes[..])?;
        if read == 0 {
            break;
        }
        for &byte in &bytes[..read] {
            for response in parser.push(char::from(byte)) {
                match response {
                    Response::Kitty => kitty = true,
                    Response::Status => return Ok(kitty),
                    _ => {}
                }
            }
        }
    }
    Ok(kitty)
}

pub async fn play(
    frames: mpsc::Receiver<Frame>,
    mut stop: watch::Receiver<bool>,
    mode: TerminalVideo,
    request_keyframe: impl Fn(),
) -> Result<()> {
    let mut command = Command::new("mpv");
    if mode == TerminalVideo::Blocks {
        command.arg("--vo=tct");
    } else {
        command.args(["--vo=kitty,tct", "--vo-kitty-alt-screen=no"]);
        if mode == TerminalVideo::KittyShm {
            command.arg("--vo-kitty-use-shm=yes");
        } else {
            // shortcut: cap base64 transfer at 320×180, revisit with compressed graphics.
            command.args([
                "--vo-kitty-use-shm=no",
                "--vo-kitty-width=320",
                "--vo-kitty-height=180",
            ]);
        }
        if std::env::var_os("TMUX").is_some() {
            command.arg("--vo-kitty-auto-multiplexer-passthrough=yes");
        }
    }
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

pub fn toggle(call: &Arc<crate::call::Call>) {
    #[cfg(not(target_os = "linux"))]
    call.state
        .lock()
        .notify("Desktop sharing requires Linux and a ScreenCast portal".into());
    #[cfg(target_os = "linux")]
    {
        let mut capture = call.video.capture.lock();
        if let Some(stop) = capture.as_ref() {
            call.video.sharing.send_replace(false);
            stop.send_replace(true);
            return;
        }
        let (stop, quit) = watch::channel(false);
        *capture = Some(stop);
        let call = call.clone();
        tokio::spawn(async move {
            let result = capture_desktop(&call, quit).await;
            call.video.sharing.send_replace(false);
            *call.video.capture.lock() = None;
            call.state.lock().notify(match result {
                Ok(()) => "Desktop sharing stopped".into(),
                Err(error) => format!("Desktop sharing: {error:#}"),
            });
        });
    }
}

#[cfg(target_os = "linux")]
async fn capture_desktop(call: &crate::call::Call, mut stop: watch::Receiver<bool>) -> Result<()> {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    let portal = Screencast::new()
        .await
        .context("ScreenCast portal unavailable")?;
    let session = portal.create_session(Default::default()).await?;
    let capture = async {
        let sources =
            portal.available_source_types().await? & (SourceType::MONITOR | SourceType::WINDOW);
        ensure!(
            !sources.is_empty(),
            "The portal does not offer screens or windows"
        );
        let mut options = SelectSourcesOptions::default().set_sources(sources);
        if portal
            .available_cursor_modes()
            .await
            .is_ok_and(|m| m.contains(CursorMode::EMBEDDED))
        {
            options = options.set_cursor_mode(CursorMode::EMBEDDED);
        }
        portal.select_sources(&session, options).await?.response()?;
        let streams = portal
            .start(&session, None, Default::default())
            .await?
            .response()?;
        let node = streams
            .streams()
            .first()
            .context("No screen selected")?
            .pipe_wire_node_id();
        let mut viewers = call.video.viewers.subscribe();
        let mut closed = session.receive_closed().await?;
        {
            let mut state = call.state.lock();
            if state.closed || !call.video.announce_share() {
                return Ok(());
            }
            state.notify("Desktop sharing is available to viewers".into());
        }
        loop {
            tokio::select! {
                _ = futures_util::StreamExt::next(&mut closed) => return Ok(()),
                ready = viewers.wait_for(|n| *n != 0) => { ready?; }
            }
            let fd = portal
                .open_pipe_wire_remote(&session, Default::default())
                .await?;
            // shortcut: repeat idle frames to bound the next keyframe without restarting
            // the encoder; replace the keepalive with on-demand refresh if idle CPU matters.
            let mut command = Command::new("gst-launch-1.0");
            command
                .args([
                    "-q",
                    "pipewiresrc",
                    "fd=0",
                    &format!("path={node}"),
                    "do-timestamp=true",
                    "keepalive-time=100",
                ])
                .args(VP8_PIPELINE)
                .stdin(Stdio::from(fd));
            tokio::select! {
                _ = futures_util::StreamExt::next(&mut closed) => return Ok(()),
                idle = viewers.wait_for(|n| *n == 0) => { idle?; }
                result = run_encoder(command, &call.video.frames, call.started) => return result,
            }
        }
    };
    let result = tokio::select! {
        _ = stop.wait_for(|stop| *stop) => Ok(()),
        result = capture => result,
    };
    call.video.sharing.send_replace(false);
    let _ = session.close().await;
    result
}

#[cfg(target_os = "linux")]
pub(crate) async fn run_encoder(
    mut command: Command,
    frames: &broadcast::Sender<Frame>,
    started: Instant,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Could not start gst-launch-1.0")?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (result, tail) = tokio::join!(
        async {
            let result = read_gdp(stdout, frames, started).await;
            if tokio::time::timeout(Duration::from_millis(200), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
            result
        },
        async {
            let mut tail = Vec::new();
            let mut buf = [0; 1024];
            while let Ok(n @ 1..) = stderr.read(&mut buf).await {
                tail.extend_from_slice(&buf[..n]);
                tail.drain(..tail.len().saturating_sub(2048));
            }
            tail
        }
    );
    let tail: String = String::from_utf8_lossy(&tail)
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    if tail.trim().is_empty() {
        result
    } else {
        result.context(format!(
            "GStreamer: {}",
            tail.split_whitespace().collect::<Vec<_>>().join(" ")
        ))
    }
}

#[cfg(target_os = "linux")]
async fn read_gdp(
    input: impl tokio::io::AsyncRead + Unpin,
    frames: &broadcast::Sender<Frame>,
    started: Instant,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, BufReader};
    let mut input = BufReader::new(input);
    let mut header = [0; 62];
    let mut origin = None;
    // GDP keeps the encoder's frame boundaries and presentation timestamps over stdout.
    loop {
        input
            .read_exact(&mut header)
            .await
            .context("Capture ended while reading a frame header")?;
        ensure!(header[..2] == [1, 0], "Unsupported capture framing");
        let len = u32::from_be_bytes(header[6..10].try_into()?) as usize;
        ensure!(len <= MAX_FRAME, "Capture packet is too large");
        let mut data = vec![0; len];
        input
            .read_exact(&mut data)
            .await
            .context("Capture ended while reading frame data")?;
        if header[4..6] == [0, 1] && !data.is_empty() {
            let nanos = u64::from_be_bytes(header[10..18].try_into()?);
            ensure!(nanos != u64::MAX, "Capture frame has no timestamp");
            let (first_pts, call_ticks) = *origin
                .get_or_insert_with(|| (nanos, started.elapsed().as_micros() as u64 * 9 / 100));
            let _ = frames.send(Frame {
                at: Instant::now(),
                ticks: call_ticks + (nanos.saturating_sub(first_pts) / 1000) * 9 / 100,
                data: data.into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_capture_cannot_announce_sharing() {
        let video = Video::default();
        let (stop, _) = watch::channel(false);
        *video.capture.lock() = Some(stop);
        assert!(video.announce_share());
        video.stop();
        assert!(!video.announce_share());
        assert!(!*video.sharing.borrow());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn encoder_error_retains_only_the_end_of_stderr() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'discard-me %05000d\\nPipeWire denied\\n' 0 >&2; exit 1",
        ]);
        let frames = Video::default().frames;
        let error = run_encoder(command, &frames, Instant::now())
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("PipeWire denied"));
        assert!(!error.contains("discard-me"));
        assert!(error.contains("reading a frame header"));
    }

    #[cfg(unix)]
    #[test]
    fn kitty_requires_a_successful_graphics_reply() {
        use std::{io::Write, os::unix::net::UnixStream};
        for (reply, supported) in [
            (&b"\x1b_Gi=31;OK\x1b\\\x1b[0n"[..], true),
            (&b"\x1b_Gi=31;EINVAL\x1b\\\x1b[0n"[..], false),
            (&b"\x1b[0n"[..], false),
        ] {
            let (input, mut terminal) = UnixStream::pair().unwrap();
            terminal.write_all(reply).unwrap();
            assert_eq!(
                read_kitty_reply(input, Duration::from_millis(100)).unwrap(),
                supported
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn silent_terminal_does_not_keep_reading_input() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixStream,
        };
        let (mut input, mut terminal) = UnixStream::pair().unwrap();
        assert!(!read_kitty_reply(&input, Duration::from_millis(5)).unwrap());
        terminal.write_all(b"v").unwrap();
        input
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut key = [0];
        input.read_exact(&mut key).unwrap();
        assert_eq!(&key, b"v");
    }

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
