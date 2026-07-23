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

/// Emit the full-distribution histogram every Nth sample (~5 min at the 30s
/// reconcile cadence): often enough to build the bimodal picture over hours, rare
/// enough to keep the journal light. The per-sample candidate line stays every tick.
const HISTOGRAM_EVERY: u64 = 10;

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

/// The eviction prior AND the display label for a victim. Deterministic: a Claude
/// session is Precious and carries its rich session label; everything else is
/// Ordinary and is named as well as it can be — by its largest process when it is a
/// terminal child, by its plain app name otherwise.
///
/// The rank comes from ONE identity read, and that is load-bearing:
/// `claude_session_label` does live IO (`/proc/*/comm`, `/proc/*/cwd`), so asking it
/// twice — once for the rank, once for the label — let a mid-kill process-set change
/// make the two disagree, a victim ranked Precious but shown with a plain name or the
/// reverse. The Ordinary branch therefore labels via `largest_proc_label`, which
/// deliberately does NOT re-ask the Claude question; only the rank may decide it.
///
/// Why the Ordinary branch needs its own label at all: a terminal child's `name` is
/// the useless generic "Terminal (child)", so a non-Claude hog inside one — a dev
/// server, a notebook, a training run — was killed and announced with no identity
/// whatsoever. The kill notification is the ONE message that has to survive being
/// read cold, hours later, with the process already gone; "killed Terminal (child)
/// (5.9GB)" is unactionable, and it named the eviction path's own blind spot rather
/// than the victim. The freeze journal had been printing "MainThread · web" for that
/// same scope all evening — the label existed, the kill path just never asked for it.
///
/// This is the generalised, extensible home for the rule the eviction effector used
/// to hard-code as an `is_claude` bool — the seam where a richer stance ("a video
/// call is Precious; a paused download is Ordinary") lands later without touching
/// the effector.
pub fn assess(path: &Path, name: String) -> (Restorability, String) {
    match cgroup::claude_session_label(path) {
        Some(label) => (Restorability::Precious, label),
        None => (Restorability::Ordinary, ordinary_label(path, name)),
    }
}

/// Best available name for a non-Claude victim. Terminal children (vte-spawn /
/// tmux-spawn) resolve to their largest process; every other scope keeps its app
/// name, which is already the better label — "Google-chrome" beats naming Chrome
/// after whichever renderer happened to be biggest.
fn ordinary_label(path: &Path, name: String) -> String {
    let raw = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if raw.starts_with("vte-spawn") || raw.starts_with("tmux-spawn") {
        cgroup::largest_proc_label(path).unwrap_or(name)
    } else {
        name
    }
}

/// Observation-only CPU-quiescence sampler — the measurement that must come before
/// the Idle class. It reads each sizeable app scope's `cpu.stat` usage delta over
/// the judgment window and records two things: the idle tail (named, every sample)
/// and, on a slower cadence, a histogram of the *whole* activity distribution. It
/// deliberately actuates **nothing**: no `cpu.idle`, no eviction bias. Its whole job
/// is to put real idle-vs-active numbers in the journal so the threshold that
/// *would* produce Idle can be derived from data, the same way the fault threshold
/// was.
///
/// Why the histogram: the candidate line only ever shows scopes below
/// `QUIESCENCE_OBS_FRACTION`, i.e. one side of the boundary. The Idle threshold
/// lives in the *valley* between the idle pile-up and the active tail, which is
/// invisible unless the active side is recorded too. The histogram is that other
/// half of the measurement.
pub struct ActivityMeter {
    /// Last-seen `usage_usec` per scope, for the delta. A scope absent this pass
    /// (ended) simply drops out — the map is rebuilt each sample.
    last_usage: HashMap<PathBuf, u64>,
    /// Sample counter, so the full-distribution histogram emits on a slower cadence
    /// (every `HISTOGRAM_EVERY`) than the per-sample candidate line.
    samples: u64,
}

impl ActivityMeter {
    pub fn new() -> Self {
        Self { last_usage: HashMap::new(), samples: 0 }
    }

    /// Sample utilisation, name the idle tail, and periodically log the full
    /// distribution. `window_secs` is the nominal cadence between calls (the
    /// reconcile's 30s). First sight of a scope yields no delta, so it is measured
    /// from the next pass on. The candidate line is quiet when nothing looks idle.
    pub fn observe(&mut self, window_secs: u64) {
        let window_usec = window_secs.max(1) as f64 * 1_000_000.0;
        let mut fresh: HashMap<PathBuf, u64> = HashMap::new();
        let mut candidates: Vec<(String, f64)> = Vec::new();
        let mut all_fracs: Vec<f64> = Vec::new();

        for app in cgroup::list_apps(JUDGMENT_MIN_SCOPE) {
            let Some(usage) = cgroup::read_cpu_usage_usec(&app.path) else {
                continue;
            };
            if let Some(&prev) = self.last_usage.get(&app.path) {
                let frac = usage.saturating_sub(prev) as f64 / window_usec;
                all_fracs.push(frac);
                if frac < QUIESCENCE_OBS_FRACTION {
                    candidates.push((idle_label(&app), frac));
                }
            }
            fresh.insert(app.path.clone(), usage);
        }
        self.last_usage = fresh;
        self.samples = self.samples.wrapping_add(1);

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

        // The other half of the measurement: the full distribution over ALL sizeable
        // scopes, so the active cluster is on record too and the idle/active valley
        // becomes findable. Slower cadence to keep the journal light.
        if self.samples % HISTOGRAM_EVERY == 0 && !all_fracs.is_empty() {
            let h = activity_histogram(&all_fracs);
            eprintln!(
                "activity dist over {}s (n={}): <1%:{} 1-2%:{} 2-5%:{} 5-10%:{} \
                 10-25%:{} 25-50%:{} >=50%:{} [Idle threshold unmeasured — the cutoff \
                 is the valley between the idle pile-up and the active tail]",
                window_secs,
                all_fracs.len(),
                h[0], h[1], h[2], h[3], h[4], h[5], h[6]
            );
        }
    }
}

/// Bucket activity fractions (of one core) into fixed bins, fine near the low end
/// where the Idle threshold will fall: `<1, 1-2, 2-5, 5-10, 10-25, 25-50, >=50 %`.
/// Half-open `[lo, hi)`, so exactly 1% lands in the 1-2% bin. Deltas are computed
/// from a saturating subtraction, so a fraction is never negative; a multi-core
/// scope (>1.0) lands in the top bin.
fn activity_histogram(fracs: &[f64]) -> [usize; 7] {
    const EDGES: [f64; 6] = [0.01, 0.02, 0.05, 0.10, 0.25, 0.50];
    let mut bins = [0usize; 7];
    for &f in fracs {
        let idx = EDGES.iter().position(|&e| f < e).unwrap_or(EDGES.len());
        bins[idx] += 1;
    }
    bins
}

/// Readable label for a candidate-idle scope: a terminal child (a vte/tmux spawn)
/// resolves to its rich session label — "claude · dir" — instead of the generic
/// "Terminal (child)"; everything else keeps its humanized name. Mirrors the
/// eviction path's `display_name`, so the journal reads the same as the kill witness.
fn idle_label(app: &cgroup::AppInfo) -> String {
    if app.raw.starts_with("vte-spawn") || app.raw.starts_with("tmux-spawn") {
        cgroup::proc_label(&app.path).unwrap_or_else(|| app.name.clone())
    } else {
        app.name.clone()
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

    /// The kill-witness regression, pinned. A non-Claude hog inside a terminal child
    /// used to be announced as the generic "Terminal (child)", because `assess` only
    /// ever consulted `claude_session_label` — so a 5.9GB dev server died with no
    /// identity in the one notification that most needed it. Terminal children now
    /// route through the largest-process label; every other scope keeps its app name,
    /// which is already better than naming Chrome after a renderer.
    ///
    /// Both paths use unreadable paths on purpose: the point is which *branch* is
    /// taken and that an empty read degrades to the fallback name rather than
    /// panicking or yielding an empty label.
    #[test]
    fn a_terminal_child_is_never_announced_as_the_generic_name_when_a_label_exists() {
        let spawn = PathBuf::from("/sys/fs/cgroup/.../tmux-spawn-deadbeef.scope");
        let vte = PathBuf::from("/sys/fs/cgroup/.../vte-spawn-deadbeef.scope");
        let app = PathBuf::from("/sys/fs/cgroup/.../app-gnome-Google-chrome-1.scope");
        // Unreadable spawn scope -> the fallback name, never a panic or "".
        assert_eq!(ordinary_label(&spawn, "Terminal (child)".to_string()), "Terminal (child)");
        assert_eq!(ordinary_label(&vte, "Terminal (child)".to_string()), "Terminal (child)");
        // A plain app scope must NOT be renamed after its largest process.
        assert_eq!(ordinary_label(&app, "Google-chrome".to_string()), "Google-chrome");
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

    #[test]
    fn histogram_records_both_the_idle_pileup_and_the_active_tail() {
        // An idle cluster (<1% and 1-2%), a gap, then an active tail — the shape the
        // Idle threshold is derived from. The old candidate line could see only the
        // first two bins; this sees all of it.
        let fracs = [0.001, 0.008, 0.015, 0.012, 0.30, 1.2];
        assert_eq!(activity_histogram(&fracs), [2, 2, 0, 0, 0, 1, 1]);
        //                                      <1 1-2 2-5 5-10 10-25 25-50 >=50
    }

    #[test]
    fn histogram_boundaries_are_half_open() {
        // Exactly-on-edge values fall into the upper bin: 1% -> 1-2%, 50% -> >=50%.
        assert_eq!(
            activity_histogram(&[0.01, 0.02, 0.05, 0.10, 0.25, 0.50]),
            [0, 1, 1, 1, 1, 1, 1]
        );
        assert_eq!(activity_histogram(&[]), [0; 7]);
    }
}
