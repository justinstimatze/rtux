use std::path::{Path, PathBuf};

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
/// Warn (once) when at least this many Claude sessions are running — an early,
/// gentle nudge to close some before pressure ever forces a freeze or kill.
const CLAUDE_SESSION_WARN_COUNT: usize = 4;
pub struct Mitigator {
    frozen: Vec<(PathBuf, String)>,
    /// Cgroups we've SIGKILLed this episode. Kept only for the per-episode cap
    /// and to skip re-killing — a kill is not reversible, so recover() never
    /// touches these beyond clearing the list.
    killed: Vec<PathBuf>,
    /// Debounce for the "too many Claude sessions" advisory.
    advised: bool,
    /// Hogs whose oom_score_adj we've raised so a global OOM eats them instead of
    /// the session — returned to neutral on recover().
    oom_biased: Vec<PathBuf>,
    self_cgroup: Option<PathBuf>,
}

impl Mitigator {
    pub fn new() -> Self {
        Self {
            frozen: Vec::new(),
            killed: Vec::new(),
            advised: false,
            oom_biased: Vec::new(),
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

    /// Critical pressure: freeze the single largest freezable consumer.
    /// One freeze per call (the daemon calls this each poll tick), so the system
    /// gets a beat to recover after each pause before we escalate further.
    pub fn escalate(&mut self) {
        if self.frozen.len() >= MAX_FROZEN {
            return;
        }
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
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
                .any(|(p, _)| &path == p || path.starts_with(p))
            {
                continue;
            }
            if !path.join("cgroup.freeze").exists() {
                continue;
            }
            match actions::freeze_cgroup(&path) {
                Ok(_) => {
                    // Actionable, calm notification off the main loop: a click on
                    // "Resume now" thaws exactly this app; "Open rtux" raises the HUD.
                    let p = path.clone();
                    let n = display_name(&path, &name);
                    let sz = format_bytes(mem);
                    let reclaim_target = mem;
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
                        let headroom = cgroup::swap_used_fraction() < SWAP_RECLAIM_CEILING;
                        if headroom {
                            let _ = actions::reclaim_cgroup(&p, reclaim_target);
                        } else {
                            eprintln!(
                                "skipped reclaim of {} — swap {:.0}% full, no room to page out",
                                n,
                                cgroup::swap_used_fraction() * 100.0
                            );
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
                    eprintln!("froze {} ({})", disp, sz);
                    crate::events::record(format!("Paused {}", disp));
                    self.frozen.push((path, disp));
                }
                Err(e) => {
                    eprintln!("failed to freeze {}: {}", path.display(), e);
                    continue;
                }
            }
            return; // one freeze per tick
        }
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
            // One identity read yields both the rank and the display label, so a
            // mid-kill process-set change can't make them disagree.
            let (rest, label) = judgment::assess(&path, name);
            eligible.push((path, label, mem, rest));
        }
        let ranks: Vec<Restorability> = eligible.iter().map(|e| e.3).collect();
        let Some(idx) = pick_victim_index(&ranks) else {
            return; // nothing eligible to kill
        };
        let (path, label, mem, rest) = eligible.swap_remove(idx);
        match actions::kill_cgroup(&path) {
            Ok(_) => {
                // Record the swap level that justified crossing the kill gate
                // (SWAP_HIGH_WATER) — so both the journal and `ctl history` show
                // *why* a kill fired, making the 85% precipice auditable in the wild.
                let swap_pct = cgroup::swap_used_fraction() * 100.0;
                eprintln!("killed {} ({}) at swap {:.0}%", label, format_bytes(mem), swap_pct);
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

    /// Pressure back to normal: undo the episode's *eviction* — thaw frozen apps
    /// and drop the OOM bias. The per-session memory caps are NOT touched here:
    /// they are a standing bound (guard::cap_active_sessions), released when a scope
    /// becomes Focused, not when a pressure episode ends. An episode is over; the
    /// rule that no background session monopolises is always on.
    pub fn recover(&mut self) {
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

        if self.frozen.is_empty() {
            return;
        }
        let count = self.frozen.len();
        for (path, name) in std::mem::take(&mut self.frozen) {
            if let Err(e) = actions::thaw_cgroup(&path) {
                eprintln!("failed to thaw {}: {}", path.display(), e);
            } else {
                eprintln!("thawed {}", name);
                crate::events::record(format!("Resumed {}", name));
            }
        }
        // No toast on recovery either — the return to normal is the good case, and
        // the per-app "thawed"/"Resumed" lines above already record it. Reserve the
        // interruption budget for the one bad, irreversible outcome: a kill.
        eprintln!("pressure cleared — resumed {} paused app(s)", count);
    }
}
