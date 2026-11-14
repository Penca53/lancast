//! Process-wide monotonic clock in microseconds, shared by capture and network code.

use std::{sync::OnceLock, time::Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the first call in this process.
pub fn now_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic() {
        let a = super::now_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(super::now_us() >= a + 2000);
    }
}
