use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch};

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
