use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::classify;
use crate::guard::format_bytes;
use crate::judgment::{self, Restorability};
use crate::{actions, cgroup, guard, notify};

/// Choose the kill victim's index from the candidates' restorability, in
/// largest-first order. Policy: take the largest `Ordinary` hog (a browser dies
/// before a Claude session); only if none remain, the largest `Precious` one. The
/// identity prior itself now lives in `judgment::restorability` — this just orders
/// by it. Returns None for an empty list.
fn pick_victim_index(rest: &[Restorability]) -> Option<usize> {
    rest.iter()
        .position(|&r| r == Restorability::Ordinary)
        .or_else(|| rest.iter().position(|&r| r == Restorability::Precious))
}

/// Choose the freeze victim's index from `(cpu_activity, size_bytes)` pairs: least
/// CPU-active first (fraction of one core), largest as the tiebreak so among
/// equally-quiescent apps the one freeing the most memory wins. Unknown activity is
/// passed as `f64::INFINITY` by the caller, so an unmeasurable cgroup is frozen only
/// when nothing measurable remains. Returns None for an empty slice.
fn pick_freeze_index(ranked: &[(f64, u64)]) -> Option<usize> {
    ranked
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.cmp(&a.1))
        })
        .map(|(i, _)| i)
}

/// Has the last freeze had its settle window (see FREEZE_SETTLE)? `None` — nothing
/// frozen yet — is always settled, so an episode's *first* freeze is never delayed;
/// the gate only ever throttles the second and later freezes of a burst.
fn settled(last_freeze: Option<Instant>, now: Instant) -> bool {
    match last_freeze {
        None => true,
        Some(t) => now.duration_since(t) >= FREEZE_SETTLE,
    }
}

/// Where a forced `memory.reclaim` on a just-frozen scope would send its pages.
#[derive(Debug, PartialEq, Eq)]
enum ReclaimPlan {
    /// zswap is on: pin the scope's writeback off, so its pages go to the
    /// compressed pool or stay put, and never to disk.
    Zswap,
    /// No zswap, but the next swap slots are on zram.
    RamSwap,
    Skip(&'static str),
}

/// The freeze already stopped the growth; reclaim is a bonus that frees RAM, and
/// it is only a bonus while the pages land in RAM. On 2026-11-20 thirteen forced
/// reclaims pushed 9.4GB through the encrypted swapfile while the compositor
/// queued behind them, because the only guard was total swap used.
fn plan_reclaim(io_full_avg10: f64, swap_frac: f64, zswap: bool, ram_next: bool) -> ReclaimPlan {
    if io_full_avg10 >= IO_FULL_RECLAIM_CEILING {
        return ReclaimPlan::Skip("the disk is already saturated");
    }
    if swap_frac >= SWAP_RECLAIM_CEILING {
        return ReclaimPlan::Skip("swap is nearly full");
    }
    if zswap {
        ReclaimPlan::Zswap
    } else if ram_next {
        ReclaimPlan::RamSwap
    } else {
        ReclaimPlan::Skip("the next swap-out would go to disk")
    }
}

/// How many times in a row this scope has been re-frozen soon after a thaw. A
/// scope thawed and re-frozen within REFREEZE_WINDOW is ping-ponging: each round
/// trip faults its whole working set back in and pushes it out again.
fn backoff_level(prev_thaw: Option<(u32, Instant)>, now: Instant) -> u32 {
    match prev_thaw {
        Some((level, at)) if now.duration_since(at) < REFREEZE_WINDOW => {
            (level + 1).min(MAX_BACKOFF_SHIFT)
        }
        _ => 0,
    }
}

/// Seconds of continuous calm a scope at `level` needs before it may thaw:
/// 10, 20, 40 ... 320.
fn calm_required(level: u32) -> u32 {
    RECOVER_AFTER_NORMAL_SECS << level.min(MAX_BACKOFF_SHIFT)
}

/// Which frozen scope to thaw next, given each one's backoff level in freeze
/// order. The lowest level whose calm requirement is met; the earliest frozen
/// among equals.
fn pick_thaw(levels: &[u32], normal_secs: u32) -> Option<usize> {
    levels
        .iter()
        .enumerate()
        .filter(|(_, &l)| calm_required(l) <= normal_secs)
        .min_by_key(|(i, &l)| (l, *i))
        .map(|(i, _)| i)
}

/// Bytes a kill would free: the footprint minus the cgroup's shmem.
///
/// Shmem charged to a cgroup is mostly tmpfs pages it wrote (in /tmp or
/// /dev/shm). Those belong to the file, not the writer, so killing the writer
/// frees none of them; a build that filled /tmp would otherwise rank as a top
/// hog and die for nothing. Some shmem (memfd, anonymous shared mappings) is
/// freed when its last mapper dies, so this undercounts browsers somewhat; the
/// error can only shrink a candidate, which is the safe direction for a kill.
fn killable_bytes(footprint: u64, shmem: u64) -> u64 {
    footprint.saturating_sub(shmem)
}

/// Thaw every unit left frozen by an earlier daemon process.
///
/// `Mitigator.frozen` lives in memory, so a restart, panic or OOM of the daemon
/// strands whatever it had paused: nothing else ever thaws it. Called once before
/// the loop starts. Thawing everything is safe because the next critical tick
/// re-freezes if pressure is still high, and it is better than adopting state
/// written by a process that just crashed. This also resumes anything paused by
/// hand with `ctl freeze`.
pub fn thaw_orphans() {
    let Ok(candidates) = cgroup::list_freezable_cgroups() else { return };
    let mut thawed = 0;
    for (path, name, mem) in candidates {
        if cgroup::read_cgroup_u64(&path, "cgroup.freeze").ok() != Some(1) {
            continue;
        }
        let scope = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let disp = display_name(&path, &name);
        match actions::thaw_cgroup(&path) {
            Ok(()) => {
                thawed += 1;
                eprintln!("startup: thawed orphaned freeze {} ({}) [{}]", disp, format_bytes(mem), scope);
                crate::events::record(format!("Resumed {} (left paused by a previous run)", disp));
            }
            Err(e) => eprintln!("startup: failed to thaw orphaned freeze {}: {}", path.display(), e),
        }
    }
    println!("pressured: startup orphan sweep thawed {} scope(s)", thawed);
}

/// The name to show in a notification / activity trail for a cgroup. Terminal
/// children (vte-spawn / tmux-spawn) resolve to their rich session label — a
/// Claude session becomes "claude · dir" — so the user sees *what* was paused or
/// killed instead of a generic "Terminal (child)". Everything else keeps its
/// plain app name.
fn display_name(path: &Path, fallback: &str) -> String {
    let raw = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if raw.starts_with("vte-spawn") || raw.starts_with("tmux-spawn") {
        cgroup::proc_label(path).unwrap_or_else(|| fallback.to_string())
    } else {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The classification predicates these tests used to exercise (hard_exempt,
    // never_freeze, and the class of a background vs focused session) now live in
    // `crate::classify`, and so do their tests — including the display-label
    // collision trap and the July HUD-bug regression. What stays here is the one
    // decision that is genuinely the eviction effector's own: victim ranking.

    #[test]
    fn victim_ranking_prefers_ordinary_then_largest_first() {
        use Restorability::{Ordinary as O, Precious as P};
        // Empty → nothing to kill.
        assert_eq!(pick_victim_index(&[]), None);
        // Only Precious (Claude) sessions → the largest (index 0) is the last resort.
        assert_eq!(pick_victim_index(&[P, P, P]), Some(0));
        // An Ordinary hog dies even when a larger Precious session precedes it.
        assert_eq!(pick_victim_index(&[P, O, P]), Some(1));
        // Largest Ordinary wins when several exist.
        assert_eq!(pick_victim_index(&[O, P, O]), Some(0));
        // A single Precious session is the victim only as the last resort, witnessed.
        assert_eq!(pick_victim_index(&[P]), Some(0));
    }

    /// The overshoot fix. A 1Hz rung steering on a 10s average froze six apps in six
    /// seconds; the gate must hold the second freeze until the sensor could have seen
    /// the first — while never delaying an episode's first freeze.
    #[test]
    fn a_freeze_waits_a_psi_window_before_the_next_one() {
        let now = Instant::now();
        // Nothing frozen yet: act immediately. Under real pressure the first freeze
        // must not be slower than it was before this gate existed.
        assert!(settled(None, now));
        // The next tick, one second later — the whole bug. avg10 cannot have moved.
        assert!(!settled(Some(now), now + Duration::from_secs(1)));
        // Just short of the window is still too early; at the window we may act.
        assert!(!settled(Some(now), now + FREEZE_SETTLE - Duration::from_millis(1)));
        assert!(settled(Some(now), now + FREEZE_SETTLE));
        // A long-idle daemon (last freeze an hour ago) is settled, not stuck.
        assert!(settled(Some(now), now + Duration::from_secs(3600)));
    }

    #[test]
    fn a_tmpfs_writer_is_not_worth_killing() {
        const G: u64 = 1024 * 1024 * 1024;
        // 3GB footprint, 2.8GB of it files in /tmp: a kill frees ~200MB.
        assert!(killable_bytes(3 * G, 2_800 * 1024 * 1024) < MIN_KILL_BYTES);
        assert_eq!(killable_bytes(2 * G, 0), 2 * G);
        assert_eq!(killable_bytes(G, 2 * G), 0);
    }

    #[test]
    fn reclaim_never_goes_to_disk() {
        // The lockup: zram full, total swap 33%, IO full 49.5%.
        assert_eq!(
            plan_reclaim(49.5, 0.33, false, false),
            ReclaimPlan::Skip("the disk is already saturated")
        );
        // A quiet disk doesn't make a disk-bound reclaim acceptable.
        assert_eq!(
            plan_reclaim(1.0, 0.33, false, false),
            ReclaimPlan::Skip("the next swap-out would go to disk")
        );
        assert_eq!(plan_reclaim(1.0, 0.33, false, true), ReclaimPlan::RamSwap);
        // zswap pins writeback off, so the slot's device doesn't matter.
        assert_eq!(plan_reclaim(1.0, 0.33, true, false), ReclaimPlan::Zswap);
        assert_eq!(plan_reclaim(1.0, 0.75, true, true), ReclaimPlan::Skip("swap is nearly full"));
    }

    #[test]
    fn a_quick_refreeze_doubles_the_calm_needed_to_thaw() {
        let now = Instant::now();
        assert_eq!(backoff_level(None, now), 0);
        assert_eq!(backoff_level(Some((0, now)), now + Duration::from_secs(30)), 1);
        assert_eq!(backoff_level(Some((2, now)), now + Duration::from_secs(30)), 3);
        // Long after its last thaw, a re-freeze is a new episode.
        assert_eq!(backoff_level(Some((4, now)), now + REFREEZE_WINDOW), 0);
        // Capped.
        assert_eq!(backoff_level(Some((MAX_BACKOFF_SHIFT, now)), now), MAX_BACKOFF_SHIFT);
        assert_eq!(calm_required(0), 10);
        assert_eq!(calm_required(3), 80);
        assert_eq!(calm_required(99), 320);
    }

    #[test]
    fn thaws_one_at_a_time_lowest_backoff_first() {
        assert_eq!(pick_thaw(&[], 100), None);
        // Nobody has had enough calm yet.
        assert_eq!(pick_thaw(&[1, 2], 15), None);
        // Earliest-frozen among equals.
        assert_eq!(pick_thaw(&[0, 0], 10), Some(0));
        // A ping-ponging scope waits behind a fresh one frozen after it.
        assert_eq!(pick_thaw(&[2, 0], 10), Some(1));
        assert_eq!(pick_thaw(&[2], 39), None);
        assert_eq!(pick_thaw(&[2], 40), Some(0));
    }

    #[test]
    fn freeze_targets_the_idlest_then_the_largest() {
        // (cpu activity as fraction-of-core, size bytes).
        // A busy 4GB app vs an idle 800MB one → freeze the idle one, felt-invisible,
        // even though it frees less: pausing a session mid-work is the worst outcome.
        assert_eq!(pick_freeze_index(&[(0.9, 4_000), (0.01, 800)]), Some(1));
        // Among equally-quiescent apps, the largest yields the most memory.
        assert_eq!(pick_freeze_index(&[(0.0, 800), (0.0, 4_000), (0.0, 2_000)]), Some(1));
        // An episode's first tick: no prior CPU samples, so all unknown (INFINITY) —
        // degrade cleanly to the old largest-first behaviour.
        let inf = f64::INFINITY;
        assert_eq!(pick_freeze_index(&[(inf, 800), (inf, 4_000)]), Some(1));
        // A measurable idle app is preferred over an unmeasurable (unknown) one.
        assert_eq!(pick_freeze_index(&[(inf, 9_000), (0.2, 800)]), Some(1));
        // Empty → nothing to freeze.
        assert_eq!(pick_freeze_index(&[]), None);
    }
}

/// Don't bother freezing anything smaller than this — the churn isn't worth it.
/// Public so the HUD flags "the app I'd pause first" against the same floor it
/// actually uses (otherwise the top-consumer marker promises a pause that never comes).
pub const MIN_FREEZE_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
/// Cap how many cgroups we'll freeze in one pressure episode. Generous on
/// purpose: freezing is REVERSIBLE (pause + reclaim-to-zram, then auto-resume
/// exactly where it left off), so pausing many background hogs is the gentle
/// workhorse — always preferred over the destructive kill rung. On a box running
/// a dozen background Claude sessions, 3 was far too low: it froze 3, then culled
/// the rest. The foreground is always spared, so freezing broadly is safe.
const MAX_FROZEN: usize = 24;
/// Don't SIGKILL anything smaller than this — same floor as freezing.
const MIN_KILL_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
/// Cap how many cgroups we'll SIGKILL in one pressure episode — a backstop so a
/// misjudged episode can't cull the whole session.
const MAX_KILLED: usize = 3;
/// Swap-used fraction at which reclaim-to-zram is futile (nowhere left to move
/// pages) and we jump straight to the kill rung, even before freezing is spent.
const SWAP_HIGH_WATER: f64 = 0.85;
/// Stop forcing memory.reclaim once swap is this full. Above it there is nowhere
/// left to page to, so a forced reclaim only stalls — and can push the kernel over
/// the edge into a global OOM (see the reclaim call site). Well below
/// SWAP_HIGH_WATER on purpose: we want to stop *shoving* long before we conclude
/// the machine is out of room and reach for the kill rung.
const SWAP_RECLAIM_CEILING: f64 = 0.70;
/// Skip forced reclaim when IO PSI `full` avg10 is at or above this: every task
/// on the box is already stalled on IO that share of the time, and a reclaim
/// that touches the disk queues the compositor's own page-ins behind it. 49.5%
/// at the 2026-11-20 lockup; ~2% on this box at rest.
const IO_FULL_RECLAIM_CEILING: f64 = 10.0;
/// Never force more than this out of one scope in one go. The old target was
/// the scope's whole footprint (up to 4GB in one write).
const RECLAIM_CAP_BYTES: u64 = 1024 * 1024 * 1024;
/// Base calm before a frozen scope may thaw. PSI is a ~10s average that can
/// rebound within seconds, so thawing on the first normal tick flapped (once
/// thawing and going critical again within 6s). Doubles per back-to-back
/// re-freeze of the same scope (see backoff_level).
pub const RECOVER_AFTER_NORMAL_SECS: u32 = 10;
/// A re-freeze within this long after a thaw counts as ping-pong.
const REFREEZE_WINDOW: Duration = Duration::from_secs(300);
/// Cap on the doubling: 10s << 5 = 320s.
const MAX_BACKOFF_SHIFT: u32 = 5;
/// Thaw one scope at a time, this far apart, so the swap-ins of several apps
/// waking at once don't land on the disk together.
const THAW_STAGGER: Duration = Duration::from_secs(5);
/// Warn (once) when at least this many Claude sessions are running — an early,
/// gentle nudge to close some before pressure ever forces a freeze or kill.
const CLAUDE_SESSION_WARN_COUNT: usize = 4;
/// Wait this long after a freeze before freezing anything else.
///
/// The daemon ticks at 1Hz but the signal it steers on is `memory.some.avg10` — a
/// **10-second** moving average. A freeze cannot show up in that number for ~10s,
/// so a per-tick freeze rung is an open loop: it keeps acting while waiting to
/// observe the effect of what it already did, and overshoots by roughly the ratio
/// of the two cadences. Measured in the wild on 2026-07-22:
///
///     23:43:23  froze Ollama (808MB)
///     23:43:24  froze Google-chrome (840MB)
///     23:43:25  froze claude · webapp (1.2GB)
///     23:43:26  froze claude · notes (3.4GB)
///     23:43:27  froze claude · plancheck (1.0GB)
///     23:43:28  froze claude · mars (1.3GB)
///
/// Six apps in six seconds — the whole desktop paused when the first one or two
/// would very likely have done it. Over six hours that pattern ran 69 times, each
/// pausing 4–7 apps, which is what "rtux keeps pausing everything" actually is.
///
/// One settle window per freeze makes the rung a closed loop: act, wait for the
/// sensor to reflect it, then decide again. It costs nothing in the case that
/// matters — if pressure is genuinely still critical 10s later, the next freeze
/// still happens — and the kill rung is deliberately NOT gated by it, so a true
/// runaway is still caught at the same speed as before.
const FREEZE_SETTLE: Duration = Duration::from_secs(10);

pub struct Mitigator {
    /// (cgroup, display name, backoff level), in freeze order.
    frozen: Vec<(PathBuf, String, u32)>,
    /// Recently thawed scopes and the backoff level they thawed at, so a quick
    /// re-freeze escalates its next calm requirement. Pruned past REFREEZE_WINDOW.
    thawed: HashMap<PathBuf, (u32, Instant)>,
    last_thaw: Option<Instant>,
    /// Cgroups we've SIGKILLed this episode. Kept only for the per-episode cap
    /// and to skip re-killing — a kill is not reversible, so recover() never
    /// touches these beyond clearing the list.
    killed: Vec<PathBuf>,
    /// Debounce for the "too many Claude sessions" advisory.
    advised: bool,
    /// Hogs whose oom_score_adj we've raised so a global OOM eats them instead of
    /// the session — returned to neutral on recover().
    oom_biased: Vec<PathBuf>,
    /// Last-seen cpu.stat usage_usec per candidate + when, so escalate() can rank
    /// freeze targets by recent CPU activity. Kept across episodes on purpose: a
    /// long gap since the last sample just yields a longer-window average, the
    /// "idle for a while" signal we want on an episode's first freeze.
    cpu_seen: HashMap<PathBuf, (u64, Instant)>,
    /// When the last freeze landed, so the next one waits a full PSI window for
    /// the signal to reflect it (see FREEZE_SETTLE). `None` until the first freeze.
    last_freeze: Option<Instant>,
    self_cgroup: Option<PathBuf>,
}

impl Mitigator {
    pub fn new() -> Self {
        Self {
            frozen: Vec::new(),
            thawed: HashMap::new(),
            last_thaw: None,
            killed: Vec::new(),
            advised: false,
            oom_biased: Vec::new(),
            cpu_seen: HashMap::new(),
            last_freeze: None,
            self_cgroup: cgroup::self_cgroup(),
        }
    }

    /// Bias the kernel's *global* OOM killer toward background hogs and away from
    /// the session — the defence for the case rtux cannot intercept at all.
    ///
    /// rtux can freeze and kill, but a global OOM (`constraint=CONSTRAINT_NONE`)
    /// fires inside the kernel with no userspace say. The only lever that reaches
    /// it is oom_score_adj, and by default the ranking here is exactly inverted:
    /// the fattest consumers (Claude sessions) self-protect at -1000 while the
    /// session's own services sit at +100/+200. On 2026-07-14 that handed the
    /// kernel a menu containing only the desktop, and it took `systemd --user` —
    /// the logout. Raising the hogs gives the killer a resumable victim instead.
    ///
    /// Re-applied every pass on purpose: oom_score_adj is per-process and inherited
    /// at fork, so a long-lived session's new children need it too.
    pub fn bias_oom_toward_hogs(&mut self) {
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
        for (path, name, mem) in candidates {
            if mem < MIN_FREEZE_BYTES {
                break; // sorted largest-first
            }
            if self.denied(&name, &path) {
                continue;
            }
            guard::bias_hog_oom(&path);
            if !self.oom_biased.iter().any(|p| p == &path) {
                self.oom_biased.push(path);
            }
        }
    }

    /// True when the eviction effector must leave this cgroup alone. Now a single
    /// class question routed through `classify`, plus one mechanism guard: the
    /// Guaranteed spine and the Focused workload are `protected_from_eviction`, and
    /// we additionally never touch the daemon's own cgroup subtree. The old
    /// hand-rolled OR of hard_exempt / foreground / touched-recently *was* this
    /// class question; it just wasn't named, and `ipc` re-derived it a slightly
    /// different way — the seam behind the July HUD bug.
    fn denied(&self, name: &str, path: &Path) -> bool {
        let raw = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Guaranteed spine — structural and cheap; short-circuit before any IO.
        if classify::hard_exempt(name, &raw) {
            return true;
        }
        // Never act on ourselves, an ancestor of us, or a descendant of us. A
        // mechanism guard, not a class question: the daemon's own name is already
        // Guaranteed, but a child scope it spawned may not match that name.
        if let Some(self_cg) = &self.self_cgroup {
            if self_cg.starts_with(path) || path.starts_with(self_cg) {
                return true;
            }
        }
        // Focused — the dynamic attention overlay: the foreground window (and its
        // tabs), or a pane a human just typed in (the only signal that survives
        // tmux, where a pane descends from the server not the focused window).
        // Resolved only now, since `observe` does IO and the cheap checks above
        // have already ruled out the spine and ourselves.
        classify::classify(&classify::observe(name, &raw, path)).protected_from_eviction()
    }

    /// Critical pressure: pause one big freezable consumer — the *idlest* one, not
    /// merely the largest. Freezing a session mid-response is the worst felt outcome;
    /// an idle background one is invisible. One freeze per call (the daemon calls this
    /// each poll tick), so the system gets a beat to recover before we escalate again.
    pub fn escalate(&mut self) {
        if self.frozen.len() >= MAX_FROZEN {
            return;
        }
        // Closed loop, not open: give the previous freeze a full PSI window to show
        // up in the average we steer on before deciding we need another one.
        let now = Instant::now();
        if !settled(self.last_freeze, now) {
            return;
        }
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Eligible: big enough to be worth it, not protected, not already
        // (nested-)frozen, and actually freezable.
        let mut eligible: Vec<(PathBuf, String, u64)> = Vec::new();
        for (path, name, mem) in candidates {
            if mem < MIN_FREEZE_BYTES {
                break; // sorted largest-first — nothing bigger remains
            }
            if self.denied(&name, &path) {
                continue;
            }
            // Skip if already frozen, or nested under something we already froze.
            if self
                .frozen
                .iter()
                .any(|(p, _, _)| &path == p || path.starts_with(p))
            {
                continue;
            }
            if !path.join("cgroup.freeze").exists() {
                continue;
            }
            // A local server with a request in flight is not a freeze candidate.
            // cgroup.freeze is SIGSTOP with no timeout, so its peer waits forever —
            // and here the peers are the agent sessions this same rung is pausing.
            // Idle servers stay eligible on purpose; they are the cheapest memory on
            // the box (a model server frozen and reclaimed went 802MB -> 7.3MB, and
            // its pages return only on demand). This spares in-flight work, not
            // servers. Checked last: it is the only filter that walks /proc.
            if cgroup::serving_local_clients(&path) {
                continue;
            }
            eligible.push((path, name, mem));
        }

        // Idle-biased ordering: among the eligible, freeze the one using the least
        // CPU right now, largest-first as the tiebreak so equally-quiescent apps
        // still yield the most memory. An *ordering*, not an Idle threshold (that
        // calibrated cutoff is the measure-first phase-5 follow-up): freeze is
        // reversible and one-per-tick, so a slightly-off pick self-corrects on the
        // next tick. Unmeasurable cgroups sort last (INFINITY). On an episode's first
        // tick nothing has a prior CPU sample, so every activity is unknown and this
        // degrades cleanly to the old largest-first behaviour.
        let mut ranked: Vec<(f64, PathBuf, String, u64)> = eligible
            .into_iter()
            .map(|(path, name, mem)| {
                let act = self.cpu_activity(&path, now).unwrap_or(f64::INFINITY);
                (act, path, name, mem)
            })
            .collect();

        // Try the idlest first; on a freeze error fall through to the next-idlest.
        while !ranked.is_empty() {
            let view: Vec<(f64, u64)> = ranked.iter().map(|(a, _, _, m)| (*a, *m)).collect();
            let Some(idx) = pick_freeze_index(&view) else {
                return;
            };
            let (_act, path, name, mem) = ranked.swap_remove(idx);
            match actions::freeze_cgroup(&path) {
                Ok(_) => {
                    // Reclaim runs off the main loop: memory.reclaim can block, and
                    // the daemon must keep reconciling. The freeze itself already
                    // stopped the growth synchronously above.
                    let p = path.clone();
                    let n = display_name(&path, &name);
                    let sz = format_bytes(mem);
                    let reclaim_target = mem.min(RECLAIM_CAP_BYTES);
                    std::thread::spawn(move || {
                        // Hibernate the frozen app's working set to compressed RAM
                        // (zram), freeing physical RAM. Best-effort — the kernel
                        // frees what it can; it faults back in on thaw. Measure how
                        // much actually moved so the user can *witness* it — this is
                        // the most impressive thing the daemon does, and it used to
                        // happen entirely in silence.
                        let before =
                            crate::cgroup::read_cgroup_u64(&p, "memory.current").unwrap_or(0);
                        // Only reclaim while swap still has somewhere to put the pages.
                        //
                        // memory.reclaim forces the kernel to push this cgroup's anon
                        // pages out NOW. That is a gift when swap has room and a
                        // loaded gun when it doesn't: on 2026-07-14 rtux forced a
                        // 4.5GB reclaim into an already-full swap (zram 6.5/7.4G,
                        // swapfile 8.8/16G) and the kernel's global OOM killer fired
                        // in the SAME SECOND, taking systemd --user and the session
                        // with it. The mitigation caused the outage it existed to
                        // prevent. Freezing alone (SIGSTOP) already stops the growth,
                        // which is the part that matters; the reclaim is only ever a
                        // bonus, so skip it rather than shove against a full swap.
                        //
                        // Total swap used is not enough on its own: it cannot see that
                        // the fast tier is full and the push is going to disk. See
                        // plan_reclaim for the full gate.
                        let io_full = crate::psi::read_psi("/proc/pressure/io")
                            .ok()
                            .and_then(|r| r.full)
                            .map(|f| f.avg10)
                            .unwrap_or(0.0);
                        let swap_frac = cgroup::swap_used_fraction();
                        let ram_next =
                            cgroup::swapout_stays_in_ram(&cgroup::read_swaps(), reclaim_target / 1024);
                        match plan_reclaim(io_full, swap_frac, cgroup::zswap_enabled(), ram_next) {
                            ReclaimPlan::Zswap => {
                                cgroup::set_zswap_writeback(&p, false);
                                let _ = actions::reclaim_cgroup(&p, reclaim_target);
                                // A focus rescue can thaw the scope while the reclaim
                                // runs; thaw_cgroup re-enabled writeback, and the write
                                // above may have landed after it. A running app keeps
                                // the default.
                                if cgroup::read_cgroup_u64(&p, "cgroup.freeze").ok() != Some(1) {
                                    cgroup::set_zswap_writeback(&p, true);
                                }
                            }
                            ReclaimPlan::RamSwap => {
                                let _ = actions::reclaim_cgroup(&p, reclaim_target);
                            }
                            ReclaimPlan::Skip(why) => {
                                eprintln!(
                                    "skipped reclaim of {}: {} (io full {:.1}%, swap {:.0}% used)",
                                    n,
                                    why,
                                    io_full,
                                    swap_frac * 100.0
                                );
                            }
                        }
                        let after = crate::cgroup::read_cgroup_u64(&p, "memory.current")
                            .unwrap_or(before);
                        let reclaimed = before.saturating_sub(after);
                        // Say "paged out", not "moved to compressed RAM": zram is only
                        // the FIRST swap device. Once it fills (it was 6.5/7.4GB full
                        // during the 2026-07-14 outage) everything overflows to the
                        // on-disk swapfile — so the cheerful "compressed RAM" line was
                        // describing gigabytes of disk writes, which is exactly what
                        // stalled the desktop it claimed to be protecting.
                        if reclaimed > 64 * 1024 * 1024 {
                            eprintln!("paged out {} from {}", format_bytes(reclaimed), n);
                            crate::events::record(format!(
                                "Paged out {} from {}",
                                format_bytes(reclaimed),
                                n
                            ));
                        }
                    });
                    // A freeze is reversible and, when it works, invisible — so it is
                    // NOT a toast (that stream buried the one notice that matters, a
                    // kill). It lives in the journal, in `ctl history`, and in the
                    // tray/HUD's ambient state; focusing the window thaws it on the
                    // spot (guard::protect_foreground). Toasts are reserved for the
                    // one destructive, irreversible event: kill_worst.
                    let disp = display_name(&path, &name);
                    let level = backoff_level(self.thawed.get(&path).copied(), now);
                    if level == 0 {
                        eprintln!("froze {} ({})", disp, sz);
                    } else {
                        eprintln!(
                            "froze {} ({}) again soon after its thaw; it now needs {}s of calm to thaw",
                            disp,
                            sz,
                            calm_required(level)
                        );
                    }
                    crate::events::record(format!("Paused {}", disp));
                    self.frozen.push((path, disp, level));
                    self.last_freeze = Some(now);
                    return; // one freeze, then a settle window before the next
                }
                Err(e) => {
                    eprintln!("failed to freeze {}: {}", path.display(), e);
                    // The idlest was unfreezable; fall through to the next-idlest so
                    // one wedged cgroup can't stall the rung or spam the log.
                }
            }
        }
    }

    /// Recent CPU use of a cgroup as a fraction of one core, measured since we last
    /// looked and updating the snapshot in place. `None` on first sight (no prior
    /// sample to diff against) or when cpu.stat is unreadable — the caller treats
    /// that as "unknown, freeze only as a last resort". During a critical episode
    /// escalate() runs every second, so samples sit ~1s apart; across episodes the
    /// gap widens into a longer-window average, which is exactly the "has this been
    /// idle for a while" signal we want on the first freeze of a new episode.
    fn cpu_activity(&mut self, path: &Path, now: Instant) -> Option<f64> {
        let usage = cgroup::read_cpu_usage_usec(path)?;
        let (prev_usage, prev_at) = self.cpu_seen.insert(path.to_path_buf(), (usage, now))?;
        let elapsed = now.duration_since(prev_at).as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }
        let delta_usec = usage.saturating_sub(prev_usage) as f64;
        Some(delta_usec / 1_000_000.0 / elapsed)
    }

    /// The last rung before the kernel's own global OOM killer. When freezing is
    /// spent (everything freezable already frozen) or swap is nearly full (so
    /// reclaim-to-zram is futile), SIGKILL the worst *background* hog to stop the
    /// climb. The kernel's global killer picks blindly — on 2026-07-14 it took
    /// dbus and the whole session down; a deliberate, witnessed, ranked kill is
    /// strictly better.
    ///
    /// Ranked B→C→A per the user's policy: a non-Claude background hog (a browser)
    /// is killed before any Claude session, and a Claude kill is announced with
    /// its directory so the session can be resumed. The foreground terminal and
    /// the hard-exempt spine are never touched.
    pub fn kill_worst(&mut self) {
        // Kill ONLY at the true precipice: swap nearly full, so reclaim-to-zram
        // is futile and freezing more can't help — the machine genuinely cannot
        // hold its working set and something must go or the kernel's blind global
        // killer takes the session. Short of that, the (reversible) freeze rung
        // does the work; we never kill just because a freeze count was hit.
        if cgroup::swap_used_fraction() < SWAP_HIGH_WATER {
            return;
        }
        if self.killed.len() >= MAX_KILLED {
            return;
        }
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
        // Build the eligible set (largest-first, past the size floor, not exempt,
        // not the foreground, not already killed), tagging each with the judgment
        // tier's restorability so the least-precious dies first.
        let mut eligible: Vec<(PathBuf, String, u64, Restorability)> = Vec::new();
        for (path, name, mem) in candidates {
            if mem < MIN_KILL_BYTES {
                break; // sorted largest-first — nothing bigger remains
            }
            if self.denied(&name, &path) || self.killed.iter().any(|p| p == &path) {
                continue;
            }
            // Size by what a kill actually frees. See killable_bytes.
            let shmem = cgroup::read_memory_stat_field(&path, "shmem").unwrap_or(0);
            let mem = killable_bytes(mem, shmem);
            if mem < MIN_KILL_BYTES {
                continue;
            }
            // One identity read yields both the rank and the display label, so a
            // mid-kill process-set change can't make them disagree.
            let (rest, label) = judgment::assess(&path, name);
            eligible.push((path, label, mem, rest));
        }
        // Subtracting shmem can reorder the candidates; pick_victim_index wants
        // them largest-first.
        eligible.sort_by_key(|e| std::cmp::Reverse(e.2));
        let ranks: Vec<Restorability> = eligible.iter().map(|e| e.3).collect();
        let Some(idx) = pick_victim_index(&ranks) else {
            return; // nothing eligible to kill
        };
        let (path, label, mem, rest) = eligible.swap_remove(idx);
        // Read before the signal: one second after it, /proc/PID is gone and
        // whatever scratch the victim was writing has no recorded owner.
        let scratch = cgroup::scratch_hints(&path);
        match actions::kill_cgroup(&path) {
            Ok(_) => {
                // Record the swap level that justified crossing the kill gate
                // (SWAP_HIGH_WATER) — so both the journal and `ctl history` show
                // *why* a kill fired, making the 85% precipice auditable in the wild.
                let swap_pct = cgroup::swap_used_fraction() * 100.0;
                // The scope name goes in the journal (not the notification): it is the
                // only durable handle on a victim once its processes are gone. With it,
                // `journalctl` can recover when the scope started, what launched it, and
                // its lifetime CPU/memory from systemd's own teardown line — the identity
                // a label alone can't carry if labelling ever comes up empty again.
                let scope = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                eprintln!(
                    "killed {} ({}) at swap {:.0}% [{}]",
                    label,
                    format_bytes(mem),
                    swap_pct,
                    scope
                );
                // A SIGKILL runs no cleanup, so the victim's temp dirs stay on disk
                // (or in tmpfs, i.e. in RAM). Recording only; rtux never deletes.
                if !scratch.is_empty() {
                    eprintln!("killed {} left its scratch unowned: {}", label, scratch.join(" "));
                }
                crate::events::record(format!("Killed {} (swap {:.0}%)", label, swap_pct));
                let tail = if rest == Restorability::Precious {
                    " You can restart this Claude session."
                } else {
                    ""
                };
                // Critical urgency: a kill is destructive and the user MUST see
                // which process died (a Claude session can then be resumed), so
                // this one punches through Do-Not-Disturb — unlike the gentler
                // freeze/rising notices, which stay quiet by design.
                notify::notify_session(
                    "critical",
                    "Stopped a memory hog to save the session",
                    &format!(
                        "Killed {} ({}) — the machine was about to run out of memory \
                         and take the whole session down.{}",
                        label,
                        format_bytes(mem),
                        tail
                    ),
                );
                self.killed.push(path);
            }
            Err(e) => eprintln!("failed to kill {}: {}", path.display(), e),
        }
    }

    /// Early, gentle nudge: if a lot of Claude sessions are running, suggest
    /// closing some *before* pressure ever forces a freeze or kill. Debounced so
    /// it fires once per accumulation, and resets when the count drops back.
    pub fn advise_claude_sessions(&mut self) {
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
        let (mut count, mut total) = (0usize, 0u64);
        for (path, _name, mem) in &candidates {
            if *mem < 128 * 1024 * 1024 {
                continue; // Claude sessions are large; skip the long tail cheaply
            }
            if cgroup::claude_session_label(path).is_some() {
                count += 1;
                total += *mem;
            }
        }
        if count >= CLAUDE_SESSION_WARN_COUNT {
            if !self.advised {
                // Advisory, not an action rtux took — and the user has asked not to
                // be toasted for routine operation. Journal only (debounced by
                // `advised` so it says it once per accumulation), for whoever reads
                // the log; no popup.
                eprintln!(
                    "{} Claude sessions using {} — closing a few would ease pressure",
                    count,
                    format_bytes(total)
                );
                self.advised = true;
            }
        } else {
            self.advised = false;
        }
    }

    /// Is anything currently frozen? The per-tick focus rescue (main loop) gates on
    /// this so an idle machine pays nothing: `guard::thaw_foreground_related` walks
    /// system.slice and every app.slice, and that walk is only worth it when there is
    /// actually a paused scope a newly-focused window might own. It is the
    /// "frozen-count the mitigator publishes" that `thaw_foreground_related`'s own doc
    /// names as the right gate.
    pub fn has_frozen(&self) -> bool {
        !self.frozen.is_empty()
    }

    /// Pressure back to normal: undo the episode's *eviction* — thaw frozen apps
    /// and drop the OOM bias. The per-session memory caps are NOT touched here:
    /// they are a standing bound (guard::cap_active_sessions), released when a scope
    /// becomes Focused, not when a pressure episode ends. An episode is over; the
    /// rule that no background session monopolises is always on.
    ///
    /// Called on every tick once pressure has been normal for
    /// RECOVER_AFTER_NORMAL_SECS, with the length of the calm so far. Thaws at most
    /// one scope per call and at most one per THAW_STAGGER: the lowest backoff level
    /// whose calm requirement `normal_secs` meets. Thawing everything at once made
    /// the whole frozen set fault its working set back in together, which on
    /// 2026-11-20 re-created the pressure that froze it.
    pub fn recover(&mut self, normal_secs: u32) {
        // Reset the per-episode kill cap and the advisory debounce. Kills are not
        // reversible, so there is nothing to undo — just start the next episode
        // fresh.
        self.killed.clear();
        self.advised = false;

        // Hand the hogs back a neutral OOM score. We do NOT restore the -1000 they
        // set for themselves — that self-protection is what left the kernel with
        // only the session to kill.
        for path in std::mem::take(&mut self.oom_biased) {
            guard::unbias_hog_oom(&path);
        }

        let now = Instant::now();
        self.thawed.retain(|_, (_, at)| now.duration_since(*at) < REFREEZE_WINDOW);
        if self.frozen.is_empty() {
            return;
        }
        if self.last_thaw.is_some_and(|t| now.duration_since(t) < THAW_STAGGER) {
            return;
        }
        let levels: Vec<u32> = self.frozen.iter().map(|f| f.2).collect();
        let Some(i) = pick_thaw(&levels, normal_secs) else { return };
        let (path, name, level) = self.frozen.remove(i);
        self.last_thaw = Some(now);
        if let Err(e) = actions::thaw_cgroup(&path) {
            eprintln!("failed to thaw {}: {}", path.display(), e);
        } else {
            eprintln!("thawed {} ({} still paused)", name, self.frozen.len());
            crate::events::record(format!("Resumed {}", name));
        }
        self.thawed.insert(path, (level, now));
        // No toast on recovery either — the return to normal is the good case, and
        // the per-app "thawed"/"Resumed" lines above already record it. Reserve the
        // interruption budget for the one bad, irreversible outcome: a kill.
        if self.frozen.is_empty() {
            eprintln!("pressure cleared — every paused app resumed");
        }
    }
}
