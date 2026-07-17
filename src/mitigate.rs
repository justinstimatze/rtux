use std::path::{Path, PathBuf};

use crate::classify;
use crate::guard::format_bytes;
use crate::{actions, cgroup, guard, notify};

/// Summon the HUD to the foreground. Wayland only grants focus to a *fresh*
/// client, so we SIGKILL any running HUD (instant death frees the D-Bus name
/// before the relaunch can race it) and spawn a new process, which reliably
/// jumps to the front. Mirrored in `setup-hotkey.sh` and the GNOME extension.
pub const SUMMON_HUD: &str = "pkill -KILL -x pressured-hud; for i in $(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud";

/// Choose the kill victim's index from `is_claude` flags in largest-first order.
/// Policy (B→C→A): prefer the largest *non-Claude* hog (a browser dies before a
/// Claude session); only if there are none, take the largest Claude session.
/// Returns None for an empty list.
fn pick_victim_index(is_claude: &[bool]) -> Option<usize> {
    is_claude
        .iter()
        .position(|&c| !c)
        .or_else(|| is_claude.iter().position(|&c| c))
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
    fn victim_ranking_prefers_non_claude_then_largest_first() {
        // Empty → nothing to kill.
        assert_eq!(pick_victim_index(&[]), None);
        // Only Claude sessions → the largest (index 0, largest-first) is chosen.
        assert_eq!(pick_victim_index(&[true, true, true]), Some(0));
        // A non-Claude hog wins even when a larger Claude session precedes it.
        assert_eq!(pick_victim_index(&[true, false, true]), Some(1));
        // Largest non-Claude wins when several exist.
        assert_eq!(pick_victim_index(&[false, true, false]), Some(0));
        // Single Claude session → it's the victim (last resort, witnessed).
        assert_eq!(pick_victim_index(&[true]), Some(0));
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
/// Cap how many cgroups we'll throttle (memory.high) at the Elevated tier.
const MAX_THROTTLED: usize = 3;
/// Don't SIGKILL anything smaller than this — same floor as freezing.
const MIN_KILL_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
/// Cap how many cgroups we'll SIGKILL in one pressure episode — a backstop so a
/// misjudged episode can't cull the whole session.
const MAX_KILLED: usize = 3;
/// Swap-used fraction at which reclaim-to-zram is futile (nowhere left to move
/// pages) and we jump straight to the kill rung, even before freezing is spent.
const SWAP_HIGH_WATER: f64 = 0.85;
/// No single background cgroup may hold more than 1/N of RAM resident once we're
/// throttling it. An absolute, machine-relative ceiling — the whole point is that
/// it does NOT scale with the target's appetite (see the throttle call site).
const BULK_RESIDENT_DIVISOR: u64 = 4;
/// Never throttle below this, however over-budget a hog is — a cap this tight is
/// already deep reclaim, and going lower just thrashes it to no benefit.
const MIN_THROTTLE_FLOOR: u64 = 256 * 1024 * 1024;
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
    throttled: Vec<(PathBuf, String)>,
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
            throttled: Vec::new(),
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

    /// Elevated pressure: gently throttle the largest freezable consumer via
    /// memory.high (forces it to reclaim its own cold pages and slows its
    /// allocation) — one per call, before we ever resort to freezing.
    pub fn throttle(&mut self) {
        if self.throttled.len() >= MAX_THROTTLED {
            return;
        }
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
        for (path, name, mem) in candidates {
            if mem < MIN_FREEZE_BYTES {
                break;
            }
            if self.denied(&name, &path) {
                continue;
            }
            if self.throttled.iter().any(|(p, _)| &path == p)
                || self.frozen.iter().any(|(p, _)| &path == p)
            {
                continue;
            }
            // Clamp to what the MACHINE can hold — never to 90% of the hog's own
            // appetite.
            //
            // `mem / 10 * 9` is relative to the target, so the cap scales WITH the
            // thing it is supposed to restrain: the bigger a runaway grows, the
            // bigger an allowance it earns. On 2026-07-14 that produced the log
            // line "throttled claude · madrid to 20.2GB" — on a 14.8GB machine.
            // A memory.high above physical RAM is a no-op the daemon recorded as a
            // successful intervention, and the session died 50 minutes later.
            //
            // memory.high is the right lever (the kernel's own docs call it "the
            // main mechanism to control memory usage": it throttles allocation into
            // direct reclaim and never invokes the OOM killer, so an over-eager cap
            // costs the hog latency, not its life). It was only ever pointed at the
            // wrong number. Take the tighter of "back off a little" and "fit in the
            // machine", so a hog can be squeezed gently but can never negotiate
            // itself a ceiling the box cannot honour.
            let machine_cap = cgroup::total_ram_bytes()
                .map(|t| t / BULK_RESIDENT_DIVISOR)
                .unwrap_or(u64::MAX);
            let high = (mem / 10 * 9)
                .min(machine_cap)
                .max(MIN_THROTTLE_FLOOR);
            match actions::cap_cgroup(&path, high) {
                Ok(_) => {
                    let disp = display_name(&path, &name);
                    eprintln!("throttled {} to {}", disp, format_bytes(high));
                    crate::events::record(format!("Eased off {}", disp));
                    self.throttled.push((path, disp));
                }
                Err(e) => {
                    eprintln!("failed to throttle {}: {}", path.display(), e);
                    continue;
                }
            }
            return; // one per tick
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
                        let significant = reclaimed > 64 * 1024 * 1024;
                        let body = if significant {
                            // Say "paged out", not "moved to compressed RAM": zram is
                            // only the FIRST swap device. Once it fills (it was
                            // 6.5/7.4GB full during the 2026-07-14 outage) everything
                            // overflows to the on-disk swapfile — so the cheerful
                            // "compressed RAM" line was describing gigabytes of disk
                            // writes, which is exactly what stalled the desktop it
                            // claimed to be protecting. Don't promise a destination we
                            // haven't checked.
                            format!(
                                "Froze {} ({}) and paged out {} to keep the desktop \
                                 responsive. Resumes automatically when pressure \
                                 clears.",
                                n, sz, format_bytes(reclaimed)
                            )
                        } else {
                            format!(
                                "Froze {} ({}) to keep the desktop responsive. \
                                 Resumes automatically when pressure clears.",
                                n, sz
                            )
                        };
                        if significant {
                            crate::events::record(format!(
                                "Paged out {} from {}",
                                format_bytes(reclaimed),
                                n
                            ));
                        }
                        match notify::notify_action(
                            "normal",
                            "Paused a memory hog",
                            &body,
                            &[("resume", "Resume now"), ("open", "Open rtux")],
                        )
                        .as_deref()
                        {
                            Some("resume") => {
                                let _ = actions::thaw_cgroup(&p);
                            }
                            Some("open") => {
                                // New-client-per-summon: Wayland only focuses a
                                // fresh client, so kill any running HUD and spawn
                                // a new process (see setup-hotkey.sh).
                                let _ = std::process::Command::new("sh")
                                    .args(["-c", SUMMON_HUD])
                                    .spawn();
                            }
                            _ => {}
                        }
                    });
                    let disp = display_name(&path, &name);
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
        // not the foreground, not already killed), labelling Claude sessions.
        let mut eligible: Vec<(PathBuf, String, u64, bool)> = Vec::new();
        for (path, name, mem) in candidates {
            if mem < MIN_KILL_BYTES {
                break; // sorted largest-first — nothing bigger remains
            }
            if self.denied(&name, &path) || self.killed.iter().any(|p| p == &path) {
                continue;
            }
            match cgroup::claude_session_label(&path) {
                Some(label) => eligible.push((path, label, mem, true)),
                None => eligible.push((path, name, mem, false)),
            }
        }
        let flags: Vec<bool> = eligible.iter().map(|e| e.3).collect();
        let Some(idx) = pick_victim_index(&flags) else {
            return; // nothing eligible to kill
        };
        let (path, label, mem, is_claude) = eligible.swap_remove(idx);
        match actions::kill_cgroup(&path) {
            Ok(_) => {
                // Record the swap level that justified crossing the kill gate
                // (SWAP_HIGH_WATER) — so both the journal and `ctl history` show
                // *why* a kill fired, making the 85% precipice auditable in the wild.
                let swap_pct = cgroup::swap_used_fraction() * 100.0;
                eprintln!("killed {} ({}) at swap {:.0}%", label, format_bytes(mem), swap_pct);
                crate::events::record(format!("Killed {} (swap {:.0}%)", label, swap_pct));
                let tail = if is_claude {
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
                notify::notify_session(
                    "normal",
                    "A lot of Claude sessions are open",
                    &format!(
                        "{} Claude sessions are using {} — closing a few will keep the \
                         machine from running low.",
                        count,
                        format_bytes(total)
                    ),
                );
                self.advised = true;
            }
        } else {
            self.advised = false;
        }
    }

    /// Pressure back to normal: undo everything this episode — thaw frozen apps
    /// and release throttles.
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

        // Release throttles (memory.high = max). Record it so the trail closes the
        // ledger — an "Eased off X" with no matching release is a half-story.
        for (path, name) in std::mem::take(&mut self.throttled) {
            if let Err(e) = actions::uncap_cgroup(&path) {
                eprintln!("failed to un-throttle {}: {}", path.display(), e);
            } else {
                eprintln!("un-throttled {}", name);
                crate::events::record(format!("Released {}", name));
            }
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
        notify::notify_session(
            "normal",
            "Memory pressure cleared",
            &format!("Resumed {} paused app(s).", count),
        );
    }
}
