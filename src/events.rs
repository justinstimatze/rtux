//! A small in-memory log of the actions the daemon has taken, so the HUD can
//! show *what rtux just did* — the witnessed history that lets trust accrue
//! (see DESIGN.md: legibility earns the confidence to delegate). Process-wide
//! and best-effort: it's a UX affordance, never load-bearing, so a poisoned
//! lock or a full buffer just drops the oldest entry.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// How many recent actions to retain. Enough to cover a pressure episode's worth
/// of freezes/thaws without growing unbounded.
const CAP: usize = 24;

struct Event {
    at: Instant,
    text: String,
}

static LOG: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());

/// Record an action, newest-last. `text` is already user-facing ("Paused Chrome").
pub fn record(text: impl Into<String>) {
    let Ok(mut log) = LOG.lock() else { return };
    log.push_back(Event {
        at: Instant::now(),
        text: text.into(),
    });
    while log.len() > CAP {
        log.pop_front();
    }
}

/// The recent actions newest-first, as (seconds-ago, text). Seconds are computed
/// at read time so the HUD always shows a fresh relative age.
pub fn recent() -> Vec<(u64, String)> {
    let Ok(log) = LOG.lock() else { return Vec::new() };
    let now = Instant::now();
    log.iter()
        .rev()
        .map(|e| (now.saturating_duration_since(e.at).as_secs(), e.text.clone()))
        .collect()
}
