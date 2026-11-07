//! What an unlock, a clock event or "now" is stamped with (architecture 9.3): the boot it
//! happened in, the boot-time clock (which keeps counting through suspend) and the system
//! clock as read.

use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// `/proc/sys/kernel/random/boot_id`: different for every boot.
    pub boot_id: Arc<str>,
    /// `CLOCK_BOOTTIME` in milliseconds: counts through suspend, restarts at each boot.
    pub boot_ms: u64,
    /// The system clock in milliseconds since 1970, as read (possibly wrong).
    pub wall_ms: i64,
}

/// Where stamps come from. The real one reads the kernel; tests script boots, sleeps and
/// clock changes.
pub trait TimeSource: Send + Sync {
    fn now(&self) -> Stamp;
}

/// The kernel's clocks.
pub struct System;

impl TimeSource for System {
    fn now(&self) -> Stamp {
        now()
    }
}

/// A stamp from the kernel's clocks. Cheap: two clock reads; the boot ID is read once.
pub fn now() -> Stamp {
    Stamp {
        boot_id: boot_id(),
        boot_ms: boot_ms(),
        wall_ms: wall_ms(),
    }
}

pub fn wall_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn boot_id() -> Arc<str> {
    static ID: OnceLock<Arc<str>> = OnceLock::new();
    ID.get_or_init(|| {
        read_boot_id().unwrap_or_else(|| {
            // No boot ID (not Linux): one per process, so nothing is ever taken for the
            // same boot across a restart.
            format!("process-{}-{}", std::process::id(), wall_ms()).into()
        })
    })
    .clone()
}

#[cfg(target_os = "linux")]
fn read_boot_id() -> Option<Arc<str>> {
    let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let s = s.trim();
    (!s.is_empty() && crate::store::lines::safe(s)).then(|| s.into())
}

#[cfg(not(target_os = "linux"))]
fn read_boot_id() -> Option<Arc<str>> {
    None
}

/// Milliseconds on a clock that keeps counting through suspend.
pub fn boot_ms() -> u64 {
    #[cfg(target_os = "linux")]
    const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
    // macOS's CLOCK_MONOTONIC keeps counting while asleep (host development only).
    #[cfg(not(target_os = "linux"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the kernel to fill.
    if unsafe { libc::clock_gettime(CLOCK, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_id_stays_the_same_and_the_boot_clock_moves_forward() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = now();
        assert_eq!(a.boot_id, b.boot_id);
        assert!(b.boot_ms >= a.boot_ms + 4);
        assert!(a.wall_ms > 1_700_000_000_000);
        #[cfg(target_os = "linux")]
        assert_eq!(a.boot_id.len(), 36, "a kernel boot id: {}", a.boot_id);
    }
}
