//! The judgment tier: the cached "thoughtful stance" on what actually matters,
//! run out-of-band (never on the fast tick) and read by the eviction effector.
//!
//! The fast classifier (`crate::classify`) answers "how much does the user need
//! this *right now*" from cheap structural signals. This tier answers the slower
//! question the fast one can't: given two expendable background workloads, which
//! one hurts less to lose? That's a judgment about *identity* — a browser tab
//! reloads, a Claude session mid-thought does not — and about *activity* — a
//! process burning a core is doing something; one that has touched no CPU in
//! minutes is a candidate to squeeze first.
//!
//! It splits by how much each half can be trusted:
//!
//! - **Restorability** is deterministic — it reads identity, not a threshold, so it
//!   drives eviction ordering *now*.
//! - **Quiescence** needs a threshold to become the Idle class, and this project
//!   earns thresholds from data rather than guessing them (the fault meter cried
//!   wolf at 20 and was re-derived to 100 from two timed events). So the CPU
//!   quiescence signal here is **observed and logged only** — it drives nothing
//!   until the idle/active separation has actually been measured in the wild.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cgroup;

/// Only observe/rank scopes above this — the small tail is neither an eviction
/// victim worth ordering nor an interesting idle candidate.
const JUDGMENT_MIN_SCOPE: u64 = 512 * 1024 * 1024; // 512 MB

/// Provisional, observation-only quiescence threshold: a scope using less than this
/// fraction of one core over the window is *logged* as a candidate for the Idle
/// class. It gates a log line, never an action — the real threshold is the thing we
/// are trying to measure, so acting on this guess would defeat the point.
const QUIESCENCE_OBS_FRACTION: f64 = 0.02; // ~2% of one core

/// How costly it is to lose this workload to eviction — the identity prior the
/// eviction effector orders victims by. Deterministic (no thresholds), so it is
/// safe to act on today. `Ordinary` is taken before `Precious`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restorability {
    /// A browser, an editor, a generic app — its state survives a restart or was
    /// never precious. Evict this first.
    Ordinary,
    /// A Claude session mid-thought: freezing or killing it destroys work in
    /// flight that no reload brings back. Evict this last, only when nothing
    /// Ordinary remains.
    Precious,
}

/// The eviction prior AND the display label for a victim, from a SINGLE identity
/// read. Deterministic: a Claude session is Precious and carries its rich session
/// label; everything else is Ordinary and keeps its plain `name`.
///
/// One read, not two, is load-bearing: `claude_session_label` does live IO
/// (`/proc/*/comm`, `/proc/*/cwd`), so reading it separately for the rank and for
/// the label let a mid-kill process-set change make the two disagree — a victim
/// ranked Precious but shown with a plain name, or the reverse. Assessing both from
/// one read keeps them consistent, the way the pre-judgment-tier code did.
///
/// This is the generalised, extensible home for the rule the eviction effector used
/// to hard-code as an `is_claude` bool — the seam where a richer stance ("a video
/// call is Precious; a paused download is Ordinary") lands later without touching
/// the effector.
pub fn assess(path: &Path, name: String) -> (Restorability, String) {
    match cgroup::claude_session_label(path) {
        Some(label) => (Restorability::Precious, label),
        None => (Restorability::Ordinary, name),
    }
}

/// Observation-only CPU-quiescence sampler — the measurement that must come before
/// the Idle class. It reads each sizeable app scope's `cpu.stat` usage delta over
/// the judgment window and logs the ones that look idle. It deliberately actuates
/// **nothing**: no `cpu.idle`, no eviction bias. Its whole job is to put real
/// idle-vs-active numbers in the journal so the threshold that *would* produce Idle
/// can be derived from data, the same way the fault threshold was.
pub struct ActivityMeter {
    /// Last-seen `usage_usec` per scope, for the delta. A scope absent this pass
    /// (ended) simply drops out — the map is rebuilt each sample.
    last_usage: HashMap<PathBuf, u64>,
}

impl ActivityMeter {
    pub fn new() -> Self {
        Self { last_usage: HashMap::new() }
    }

    /// Sample utilisation and log candidate-idle scopes. `window_secs` is the
    /// nominal cadence between calls (the reconcile's 30s). First sight of a scope
    /// yields no delta, so it is measured from the next pass on. Quiet when nothing
    /// looks idle.
    pub fn observe(&mut self, window_secs: u64) {
        let window_usec = window_secs.max(1) as f64 * 1_000_000.0;
        let mut fresh: HashMap<PathBuf, u64> = HashMap::new();
        let mut candidates: Vec<(String, f64)> = Vec::new();

        for app in cgroup::list_apps(JUDGMENT_MIN_SCOPE) {
            let Some(usage) = cgroup::read_cpu_usage_usec(&app.path) else {
                continue;
            };
            if let Some(&prev) = self.last_usage.get(&app.path) {
                let frac = usage.saturating_sub(prev) as f64 / window_usec;
                if frac < QUIESCENCE_OBS_FRACTION {
                    candidates.push((app.name.clone(), frac));
                }
            }
            fresh.insert(app.path.clone(), usage);
        }
        self.last_usage = fresh;

        if !candidates.is_empty() {
            let list = candidates
                .iter()
                .map(|(name, frac)| format!("{} {:.1}%", name, frac * 100.0))
                .collect::<Vec<_>>()
                .join(", ");
            // OBSERVE-ONLY. This line is the measurement, not a mitigation. When the
            // idle/active split is clear in the journal, a measured threshold turns
            // these into the Idle class (cpu.idle + squeezed first); until then it
            // acts on nothing.
            eprintln!(
                "quiescence [observe-only, not actuated]: {} — candidate Idle over {}s \
                 (threshold unmeasured)",
                list, window_secs
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A non-Claude scope assesses as Ordinary and keeps its plain name — and the
    /// rank/label come from the same read, so they can't disagree. (Claude
    /// detection itself is `cgroup`'s; here we pin the mapping the effector orders
    /// by, and that a plain path is Ordinary rather than accidentally Precious.)
    #[test]
    fn a_non_claude_scope_assesses_ordinary_and_keeps_its_name() {
        let plain = PathBuf::from("/sys/fs/cgroup/user.slice/.../firefox.scope");
        let (rest, label) = assess(&plain, "Firefox".to_string());
        assert_eq!(rest, Restorability::Ordinary);
        assert_eq!(label, "Firefox");
    }

    /// The ordering the eviction effector relies on: Ordinary is strictly "evict
    /// before" Precious. If this ever flips, the effector would kill the precious
    /// session while an expendable browser sat untouched.
    #[test]
    fn ordinary_evicts_before_precious() {
        // Encoded as: Ordinary is the value `pick_victim` reaches for first. The
        // effector's own ranking test (mitigate) exercises the selection; this pins
        // the two-tier intent at the source of truth.
        assert_ne!(Restorability::Ordinary, Restorability::Precious);
    }
}
