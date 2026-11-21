use std::{
    thread,
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Clone)]
pub struct Shutdown(watch::Sender<bool>);

impl Shutdown {
    pub fn new() -> std::io::Result<Self> {
        let shutdown = Self(watch::channel(false).0);
        #[cfg(unix)]
        {
            use signal_hook::{
                consts::{SIGHUP, SIGINT, SIGTERM},
                iterator::Signals,
            };
            let mut signals = Signals::new([SIGHUP, SIGINT, SIGTERM])?;
            let shutdown = shutdown.clone();
            thread::Builder::new()
                .name("voice-signals".into())
                .spawn(move || {
                    for _ in signals.forever() {
                        shutdown.request();
                    }
                })?;
        }
        Ok(shutdown)
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }

    pub fn request(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        if self.0.send_if_modified(|set| !std::mem::replace(set, true)) {
            // Neither a blocked terminal/audio driver nor Tokio can delay this deadline.
            thread::spawn(move || {
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                #[cfg(unix)]
                signal_hook::low_level::exit(1);
                #[cfg(not(unix))]
                std::process::exit(1);
            });
        }
    }
}
