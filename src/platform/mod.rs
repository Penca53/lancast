//! Platform-specific capture and encoding. Each platform provides:
//! `monitors()`, `encoders(monitor)`, `run_video(settings, hub, stats, stop)` and `run_audio(hub, stop)`.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{encoders, monitors, run_audio, run_video};

#[cfg(not(windows))]
mod unsupported {
    use crate::{
        config::{Codec, Settings},
        server::Stats,
        stream::Hub,
    };
    use std::sync::atomic::AtomicBool;

    pub fn monitors() -> Vec<super::Monitor> {
        Vec::new()
    }
    pub fn encoders(_: &str) -> Vec<Codec> {
        Vec::new()
    }
    pub fn run_video(_: &Settings, _: &Hub, _: &Stats, _: &AtomicBool) -> anyhow::Result<()> {
        anyhow::bail!("screen capture is not implemented on this platform yet")
    }
    pub fn run_audio(_: &Hub, _: &AtomicBool) -> anyhow::Result<()> {
        anyhow::bail!("audio capture is not implemented on this platform yet")
    }
}
#[cfg(not(windows))]
pub use unsupported::{encoders, monitors, run_audio, run_video};
