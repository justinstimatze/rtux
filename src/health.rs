//! The spine's major-fault RATE — the one number that says whether rtux is
//! keeping its promise.
//!
//! # Why a rate, and never a total
//!
//! `pgmajfault` in `memory.stat` is monotonic: it counts every major fault the
//! cgroup has taken since it was created, and it never decreases. That makes the
//! total useless as a health signal, and worse than useless on a HUD. Measured on
//! this machine 2026-07-15, 22h after the incident:
//!
//!     org.gnome.Shell  pgmajfault = 182,714   <-- looks alarming
//!     org.gnome.Shell  d/dt        =       0   <-- actually perfectly healthy
//!
//! The 182,714 is a *scar* from the 2026-07-14 incident. The compositor is fine
//! now — it has taken zero major faults per minute since the app.slice ceiling
//! went in. A HUD wired to the total would show a permanently-red number that can
//! never improve no matter what rtux does, and whoever read it would go tuning
//! against damage that already healed. That is exactly the failure mode of the
//! display-string gate and the stale journal (see DESIGN.md): a confident
//! instrument reporting something that isn't true now.
//!
//! So: sample the counter each tick, keep the delta, throw the total away.
//!
//! # Why this is the metric at all
//!
//! `memory.swap.current` is a cache statistic — app.slice being swapped is the
//! *goal*, not the harm. The harm is the interactive path waiting on disk at the
//! moment the user acts. A major fault on the spine IS that wait, counted. The
//! 19s keyboard stall was "the input method was swapped AND a key was pressed";
//! this counts the second half, which is the half that hurts.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::cgroup;
use crate::guard;

/// ~last minute at the daemon's 1s poll, matching trend.rs.
const CAP: usize = 60;

/// Spine faults in one tick above which we write an incident line to the journal.
///
/// This threshold is a GUESS and is marked as one deliberately. What we know: the
/// spine idles at exactly 0 faults/s on this machine, so any sustained nonzero is
/// already abnormal, and 1-2 is plausibly just a service touching a cold page of
/// its own binary. What we do not know is where "noticeable" starts, because the
/// only incident we have was never instrumented — which is the whole reason this
/// module exists.
///
/// 20/s is picked to be quiet at idle and to fire well before a stall of the kind
/// that started this project. Once a real incident is captured, replace this with
/// the measured number and delete this paragraph. Do not tune it against a healthy
/// machine — that is guessing, and guessing is what this module is here to end.
const INCIDENT_FAULTS_PER_TICK: u64 = 20;

/// One tick of spine harm.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Major faults across the entire spine during this tick.
    pub faults: u64,
    /// The class that took the most this tick, and how many — so the journal can
    /// say *what* hurt, not merely that something did. None when the spine was
    /// clean, which is the normal case.
    pub worst: Option<(String, u64)>,
    /// How many spine cgroups this sample actually read.
    ///
    /// Carried so a reader can tell "nothing hurt" from "I couldn't look". Zero
    /// faults across zero cgroups is not a clean spine, it is a blind meter — and
    /// summing an empty set yields 0, which renders as a confident green
    /// "resident — clean". That is the exact failure of the display-string gate
    /// and the stale journal: a broken instrument handing back a healthy verdict.
    /// The HUD must be able to say "unknown", so it needs this to say it with.
    pub observed: usize,
}

/// Default is deliberately NOT derived. `Sample::default()` is what `latest()`
/// returns before the first tick, and a derived default would be
/// `{faults: 0, observed: 0}` — indistinguishable from a real clean tick only if
/// nobody checks `observed`, which is precisely why `observed` exists. Written by
/// hand so this stays a decision rather than a derive nobody read.
impl Default for Sample {
    fn default() -> Self {
        Sample { faults: 0, worst: None, observed: 0 }
    }
}

static RING: Mutex<VecDeque<Sample>> = Mutex::new(VecDeque::new());

/// Best-effort, like trend::record: a poisoned lock costs us a sample, not the
/// daemon. The protector must never die for the instrument's sake.
fn push(sample: Sample) {
    let Ok(mut r) = RING.lock() else { return };
    r.push_back(sample);
    while r.len() > CAP {
        r.pop_front();
    }
}

/// Worst tick in the retained window — "did the spine hurt in the last minute?",
/// which the instantaneous value cannot answer. A stall is over in less time than
/// it takes to open the HUD.
///
/// None when no tick in the window managed to read a single spine cgroup: an
/// unseen minute is not a quiet one. Ticks that DID observe something still count
/// even if others were blind — a partial window is a real, if incomplete, answer.
pub fn peak() -> Option<u64> {
    let r = RING.lock().ok()?;
    r.iter().filter(|s| s.observed > 0).map(|s| s.faults).max()
}

/// Most recent tick.
pub fn latest() -> Sample {
    RING.lock().ok().and_then(|r| r.back().cloned()).unwrap_or_default()
}

/// Samples per-cgroup `pgmajfault` and turns the monotonic counters into
/// per-tick deltas. Owned by the daemon loop.
pub struct FaultMeter {
    /// Last-seen counter per cgroup. Keyed by path, because a class can span
    /// several cgroups (see cgroup::find_all_cgroups_for_service).
    prev: HashMap<PathBuf, u64>,
    spine: Vec<(String, PathBuf)>,
}

impl FaultMeter {
    pub fn new() -> Self {
        let mut m = FaultMeter { prev: HashMap::new(), spine: Vec::new() };
        m.refresh_spine();
        m
    }

    /// Re-enumerate the spine. Must be called periodically for the same reason
    /// protection must be re-asserted: a re-login builds a whole new session tree,
    /// and a meter holding the old paths would read counters that stopped moving
    /// and cheerfully report a perfectly healthy spine forever.
    pub fn refresh_spine(&mut self) {
        self.spine = guard::spine_cgroups();
        // Drop counters for cgroups that no longer exist, so a path reused by a
        // fresh cgroup doesn't diff against a dead one's total and report a
        // enormous phantom spike on its first tick.
        let live: Vec<&PathBuf> = self.spine.iter().map(|(_, p)| p).collect();
        self.prev.retain(|p, _| live.contains(&p));
    }

    /// Read every spine cgroup, record one Sample, and return it.
    pub fn tick(&mut self) -> Sample {
        let mut total = 0u64;
        // Tally per CLASS, not per cgroup: "audio" spans pipewire, pipewire-pulse
        // and wireplumber, and the user experiences one audio path, not three
        // units. Tallying fully before ranking — rather than tracking a running
        // max — because a class's cgroups need not be contiguous in the walk, and
        // a running max silently drops the deltas a class took before it led.
        let mut per_class: HashMap<&str, u64> = HashMap::new();
        let mut observed = 0usize;

        for (name, path) in &self.spine {
            let Some(now) = cgroup::read_memory_stat_field(path, "pgmajfault") else { continue };
            observed += 1;
            match self.prev.insert(path.clone(), now) {
                // First sight of this cgroup: we have a total, not a delta. Seed
                // and report nothing. Counting the total here would report the
                // whole 182k scar as one tick of harm at every daemon restart.
                None => {}
                Some(before) => {
                    // saturating: a counter can only go backwards if the cgroup was
                    // recreated at the same path, in which case the "delta" is
                    // meaningless — take zero over a wrapped enormity.
                    let delta = now.saturating_sub(before);
                    if delta > 0 {
                        total += delta;
                        *per_class.entry(name.as_str()).or_default() += delta;
                    }
                }
            }
        }

        let worst = per_class
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(name, &count)| (name.to_string(), count));
        let sample = Sample { faults: total, worst, observed };
        if sample.faults >= INCIDENT_FAULTS_PER_TICK {
            // The black box. This line is the entire point of the module: the next
            // halt must leave a record even if it happens at 3am with nobody
            // watching, because the plan of record is to learn from it rather than
            // guess again.
            let who = sample
                .worst
                .as_ref()
                .map(|(n, c)| format!("{n} took {c}"))
                .unwrap_or_else(|| "no single class dominant".to_string());
            eprintln!(
                "SPINE HURT: {} major faults this second ({who}) — the interactive \
                 path is waiting on disk",
                sample.faults
            );
        }
        push(sample.clone());
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists to prevent, as a test: a fresh meter seeing a
    /// cgroup carrying a huge historical total must report zero, not the total.
    #[test]
    fn first_sight_of_a_cgroup_reports_no_harm_however_large_its_scar() {
        let mut prev: HashMap<PathBuf, u64> = HashMap::new();
        let p = PathBuf::from("/fake/gnome-shell");
        // Simulate the tick() body's first-sight arm.
        let scar = 182_714u64;
        let reported = match prev.insert(p.clone(), scar) {
            None => 0,
            Some(before) => scar.saturating_sub(before),
        };
        assert_eq!(reported, 0, "a monotonic total must never be reported as one tick of harm");
    }

    #[test]
    fn a_counter_going_backwards_reports_zero_not_a_wrapped_enormity() {
        let before = 500u64;
        let now = 3u64; // cgroup recreated at the same path
        assert_eq!(now.saturating_sub(before), 0);
    }

    #[test]
    fn peak_answers_did_it_hurt_recently_when_the_latest_tick_is_clean() {
        // A stall is over before the HUD opens; `latest` alone would say "fine".
        let samples = [Sample { faults: 0, worst: None, observed: 3 },
                       Sample { faults: 412, worst: Some(("compositor".into(), 412)), observed: 3 },
                       Sample { faults: 0, worst: None, observed: 3 }];
        let peak = samples.iter().map(|s| s.faults).max().unwrap();
        assert_eq!(samples.last().unwrap().faults, 0, "the instant reads clean...");
        assert_eq!(peak, 412, "...but the window remembers the stall");
    }

    /// The bug this module could most easily have shipped: an empty spine sums to
    /// zero faults, and zero faults renders green. A blind meter must never be
    /// mistaken for a healthy one.
    #[test]
    fn a_blind_meter_is_distinguishable_from_a_clean_spine() {
        let blind = Sample::default();
        let clean = Sample { faults: 0, worst: None, observed: 4 };
        assert_eq!(blind.faults, clean.faults, "both report zero faults...");
        assert_ne!(blind, clean, "...so `faults` alone cannot tell them apart");
        assert_eq!(blind.observed, 0, "and `observed` is what does");
    }

    /// A window of only-blind ticks must answer "I don't know", not "0".
    #[test]
    fn peak_over_a_blind_window_is_unknown_rather_than_zero() {
        let blind_window = [Sample::default(), Sample::default()];
        let peak = blind_window.iter().filter(|s| s.observed > 0).map(|s| s.faults).max();
        assert_eq!(peak, None, "an unseen minute is not a quiet minute");
    }

    /// ...but a window that saw *something* still answers, partial as it is.
    #[test]
    fn peak_survives_some_blind_ticks_if_any_tick_could_see() {
        let mixed = [Sample::default(),
                     Sample { faults: 7, worst: None, observed: 2 },
                     Sample::default()];
        let peak = mixed.iter().filter(|s| s.observed > 0).map(|s| s.faults).max();
        assert_eq!(peak, Some(7));
    }

    #[test]
    fn the_idle_spine_is_below_the_incident_threshold() {
        // Measured 2026-07-15: gnome-shell and IBus both at 0 faults/min at idle.
        // If this ever fires, either the machine changed or the threshold is wrong.
        assert!(0 < INCIDENT_FAULTS_PER_TICK);
        assert!(2 < INCIDENT_FAULTS_PER_TICK, "a cold binary page must not be an incident");
    }
}
