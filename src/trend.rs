//! A short rolling history of memory pressure, so the HUD can show pressure
//! *climbing* — you see a storm coming, not just its arrival (the anticipatory
//! thread of the vision). The daemon reads PSI every tick anyway; it records the
//! `some.avg10` here so the history predates the HUD being opened. Best-effort:
//! a poisoned lock just yields an empty history.

use std::collections::VecDeque;
use std::sync::Mutex;

/// ~last minute at the daemon's 1s poll.
const CAP: usize = 60;

static SAMPLES: Mutex<VecDeque<f64>> = Mutex::new(VecDeque::new());

/// Record one PSI `some.avg10` sample (percent).
pub fn record(some_avg10: f64) {
    let Ok(mut s) = SAMPLES.lock() else { return };
    s.push_back(some_avg10);
    while s.len() > CAP {
        s.pop_front();
    }
}

/// The recorded samples, oldest-first.
pub fn history() -> Vec<f64> {
    SAMPLES
        .lock()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}
