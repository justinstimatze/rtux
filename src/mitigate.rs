use std::path::{Path, PathBuf};

use crate::guard::format_bytes;
use crate::{actions, cgroup, guard, notify};

/// Summon the HUD to the foreground. Wayland only grants focus to a *fresh*
/// client, so we SIGKILL any running HUD (instant death frees the D-Bus name
/// before the relaunch can race it) and spawn a new process, which reliably
/// jumps to the front. Mirrored in `setup-hotkey.sh` and the GNOME extension.
pub const SUMMON_HUD: &str = "pkill -KILL -x pressured-hud; for i in $(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud";

/// Hard-exempt cgroups: the auto-mitigator must never freeze OR kill these —
/// doing so breaks the session, the display, or the daemon itself. The session
/// spine + system-critical services + PID 1 + the protector. Matched as
/// substrings against both the raw cgroup dir name and the humanized app name.
const HARD_EXEMPT_NAMES: &[&str] = &[
    // compositor / display
    "org.gnome.Shell", "gnome-shell", "kwin", "sway", "plasmashell",
    "Xorg", "Xwayland", "mutter",
    // audio
    "pipewire", "pulseaudio", "wireplumber",
    // session / system critical
    "dbus", "systemd", "gnome-keyring", "polkit", "gdm", "sddm",
    "display-manager", "NetworkManager", "sshd", "accounts-daemon", "rtkit",
    "gvfs", "xdg-desktop-portal", "gnome-session",
    // PID 1 lives here — freezing/killing it takes down the whole system.
    "init.scope",
    // the protector itself
    "rtux", "pressured",
];

/// Interactive terminals. Spared from *automatic* freeze/kill only while they're
/// the foreground (or descend from it) — see `denied`. A BACKGROUND terminal
/// session (an idle shell, a build, a Claude session you're not looking at) is a
/// legitimate target: that's where this machine's pressure actually comes from,
/// and blanket-exempting it is why rtux used to sit helpless while the session
/// climbed to a global-OOM crash. Still refused for user-initiated HUD actions
/// (never_freeze stays a union), which is the conservative default there.
const TERMINAL_NAMES: &[&str] = &[
    "vte-spawn", "gnome-terminal", "konsole", "kitty", "alacritty",
    "xterm", "tmux", "screen", "terminator",
];

fn matches_any(name: &str, raw_dir_name: &str, list: &[&str]) -> bool {
    list.iter().any(|d| raw_dir_name.contains(d) || name.contains(d))
}

/// Hard-exempt from *automatic* mitigation (freeze or kill), and from the
/// foreground protection path — the spine/system/daemon must never be pinned,
/// boosted, or relaxed as if it were an ordinary focused app. Terminals are NOT
/// hard-exempt: a focused terminal is a legitimate foreground to favour.
pub fn hard_exempt(name: &str, raw_dir_name: &str) -> bool {
    matches_any(name, raw_dir_name, HARD_EXEMPT_NAMES)
}

/// True if this cgroup must never be frozen via the IPC/HUD path. Unchanged
/// behaviour: the hard-exempt spine *and* every terminal (the conservative
/// default for user-initiated actions). The auto-mitigator uses the finer
/// `hard_exempt` + foreground checks in `denied` instead.
pub fn never_freeze(name: &str, raw_dir_name: &str) -> bool {
    hard_exempt(name, raw_dir_name) || matches_any(name, raw_dir_name, TERMINAL_NAMES)
}

/// True if `path` is the foreground window's cgroup, or hosts any process that
/// descends from the foreground window's pid — i.e. the terminal the user is in
/// and all of its tabs. Such cgroups are spared from automatic freeze/kill so we
/// never touch what the user is actively working in. Returns false when nothing
/// has reported focus yet (fail-open: better to act than to freeze on ambiguity
/// under real pressure — the hard-exempt spine is still protected regardless).
fn is_foreground_related(path: &Path) -> bool {
    let Some(fg_pid) = crate::ipc::foreground_pid() else {
        return false;
    };
    if let Some(fg_cg) = cgroup::cgroup_of_pid(fg_pid) {
        if fg_cg == path {
            return true;
        }
    }
    let Ok(procs) = std::fs::read_to_string(path.join("cgroup.procs")) else {
        return false;
    };
    procs.lines().any(|l| {
        l.trim()
            .parse::<i32>()
            .map(|pid| cgroup::pid_descends_from(pid, fg_pid))
            .unwrap_or(false)
    })
}

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
/// Stop forcing memory.reclaim once swap is this full. Above it there is nowhere
/// left to page to, so a forced reclaim only stalls — and can push the kernel over
/// the edge into a global OOM (see the reclaim call site). Well below
/// SWAP_HIGH_WATER on purpose: we want to stop *shoving* long before we conclude
/// the machine is out of room and reach for the kill rung.
const SWAP_RECLAIM_CEILING: f64 = 0.70;
/// Warn (once) when at least this many Claude sessions are running — an early,
/// gentle nudge to close some before pressure ever forces a freeze or kill.
const CLAUDE_SESSION_WARN_COUNT: usize = 4;
/// cpu.weight a background hog is demoted to under CPU contention (vs the 100
/// default and the foreground's 1000). Low enough to cede the cores to the
/// desktop + focused app; still non-zero so it isn't fully starved.
const CPU_WEIGHT_THROTTLED: u32 = 10;
/// The default cpu.weight a demoted hog is restored to when CPU pressure clears.
const CPU_WEIGHT_NORMAL: u32 = 100;
/// Skip demoting cgroups smaller than this (avoid churn on trivial ones). Most
/// sustained CPU hogs — compiles, node, browsers — are well above it.
const CPU_THROTTLE_FLOOR: u64 = 64 * 1024 * 1024; // 64 MB

pub struct Mitigator {
    frozen: Vec<(PathBuf, String)>,
    throttled: Vec<(PathBuf, String)>,
    /// Cgroups we've SIGKILLed this episode. Kept only for the per-episode cap
    /// and to skip re-killing — a kill is not reversible, so recover() never
    /// touches these beyond clearing the list.
    killed: Vec<PathBuf>,
    /// Debounce for the "too many Claude sessions" advisory.
    advised: bool,
    /// Background hogs whose cpu.weight we've demoted under CPU pressure —
    /// restored on cpu_recover(). Independent of the memory ladder.
    cpu_throttled: Vec<PathBuf>,
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
            cpu_throttled: Vec::new(),
            oom_biased: Vec::new(),
            self_cgroup: cgroup::self_cgroup(),
        }
    }

    /// CPU pressure: demote *background* hogs' cpu.weight so the desktop and the
    /// focused app keep the cores. cpu.weight is work-conserving, so this is a
    /// no-op for idle background cgroups and only bites the ones actually burning
    /// CPU — which is why we can blanket-demote without sampling who's hot. The
    /// foreground and the hard-exempt spine are spared by denied(). Reversible.
    pub fn cpu_throttle(&mut self) {
        let candidates = match cgroup::list_freezable_cgroups() {
            Ok(c) => c,
            Err(_) => return,
        };
        for (path, name, mem) in candidates {
            if mem < CPU_THROTTLE_FLOOR {
                break; // sorted largest-first
            }
            if self.denied(&name, &path) || self.cpu_throttled.iter().any(|p| p == &path) {
                continue;
            }
            cgroup::ensure_cpu_controller(&path);
            if actions::set_cpu_weight(&path, CPU_WEIGHT_THROTTLED).is_ok() {
                self.cpu_throttled.push(path);
            }
        }
    }

    /// CPU pressure cleared: restore every background hog we demoted.
    pub fn cpu_recover(&mut self) {
        if self.cpu_throttled.is_empty() {
            return;
        }
        let count = self.cpu_throttled.len();
        for path in std::mem::take(&mut self.cpu_throttled) {
            let _ = actions::set_cpu_weight(&path, CPU_WEIGHT_NORMAL);
        }
        eprintln!("restored cpu.weight on {} background app(s)", count);
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
            // Squeeze to 90% of current: reclaim a slice without heavy stalling.
            let high = (mem / 10 * 9).max(256 * 1024 * 1024);
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

    fn denied(&self, name: &str, path: &Path) -> bool {
        let raw = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // The spine/system/daemon are always off-limits (freeze OR kill).
        if hard_exempt(name, &raw) {
            return true;
        }
        // Never act on ourselves, an ancestor of us, or a descendant of us.
        if let Some(self_cg) = &self.self_cgroup {
            if self_cg.starts_with(path) || path.starts_with(self_cg) {
                return true;
            }
        }
        // Spare whatever the user is actively using: the foreground window's own
        // cgroup and anything whose processes descend from it (the terminal
        // you're in and all its tabs). Everything else in the background is fair
        // game — including background terminal/Claude sessions.
        if is_foreground_related(path) {
            return true;
        }
        // …but the check above is blind under tmux: a pane descends from the tmux
        // server, not from the focused window, so it reported the pane being typed
        // in as background and we froze it mid-keystroke. A session that says a
        // human just typed in it is spared on its own word (see ipc::LIVE), which
        // is the only signal that survives tmux. Bounded by TTL and MAX_LIVE, so
        // this can never starve the mitigator of candidates.
        if crate::ipc::touched_recently(path) {
            return true;
        }
        false
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
