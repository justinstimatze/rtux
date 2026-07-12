use std::path::{Path, PathBuf};

use crate::guard::format_bytes;
use crate::{actions, cgroup, notify};

/// Summon the HUD to the foreground. Wayland only grants focus to a *fresh*
/// client, so we SIGKILL any running HUD (instant death frees the D-Bus name
/// before the relaunch can race it) and spawn a new process, which reliably
/// jumps to the front. Mirrored in `setup-hotkey.sh` and `pressured-tray.rs`.
pub const SUMMON_HUD: &str = "pkill -KILL -x pressured-hud; for i in $(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud";

/// Cgroups we must never freeze: freezing any of these would make the desktop
/// *less* responsive, break the session, or pause the daemon itself. Matched as
/// substrings against both the raw cgroup dir name and the humanized app name.
const NEVER_FREEZE: &[&str] = &[
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
    // interactive terminals — don't freeze the user's active shell
    "vte-spawn", "gnome-terminal", "konsole", "kitty", "alacritty",
    "xterm", "tmux", "screen",
];

/// True if this cgroup must never be frozen (matched against raw dir name and
/// humanized app name). Shared by the auto-mitigator and the IPC/HUD layer so
/// both agree on what's off-limits.
pub fn never_freeze(name: &str, raw_dir_name: &str) -> bool {
    NEVER_FREEZE
        .iter()
        .any(|d| raw_dir_name.contains(d) || name.contains(d))
}

/// Don't bother freezing anything smaller than this — the churn isn't worth it.
/// Public so the HUD flags "the app I'd pause first" against the same floor it
/// actually uses (otherwise the top-consumer marker promises a pause that never comes).
pub const MIN_FREEZE_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
/// Cap how many cgroups we'll freeze in one pressure episode.
const MAX_FROZEN: usize = 3;
/// Cap how many cgroups we'll throttle (memory.high) at the Elevated tier.
const MAX_THROTTLED: usize = 3;

pub struct Mitigator {
    frozen: Vec<(PathBuf, String)>,
    throttled: Vec<(PathBuf, String)>,
    self_cgroup: Option<PathBuf>,
}

impl Mitigator {
    pub fn new() -> Self {
        Self {
            frozen: Vec::new(),
            throttled: Vec::new(),
            self_cgroup: cgroup::self_cgroup(),
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
                    eprintln!("throttled {} to {}", name, format_bytes(high));
                    crate::events::record(format!("Eased off {}", name));
                    self.throttled.push((path, name));
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
        if never_freeze(name, &raw) {
            return true;
        }
        // Never freeze ourselves, an ancestor of us, or a descendant of us.
        if let Some(self_cg) = &self.self_cgroup {
            if self_cg.starts_with(path) || path.starts_with(self_cg) {
                return true;
            }
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
                    let n = name.clone();
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
                        let _ = actions::reclaim_cgroup(&p, reclaim_target);
                        let after = crate::cgroup::read_cgroup_u64(&p, "memory.current")
                            .unwrap_or(before);
                        let reclaimed = before.saturating_sub(after);
                        let significant = reclaimed > 64 * 1024 * 1024;
                        let body = if significant {
                            format!(
                                "Froze {} ({}) and moved {} to compressed RAM to keep \
                                 the desktop responsive. Resumes automatically when \
                                 pressure clears.",
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
                                "Reclaimed {} from {} to compressed RAM",
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
                    crate::events::record(format!("Paused {}", name));
                    self.frozen.push((path, name));
                }
                Err(e) => {
                    eprintln!("failed to freeze {}: {}", path.display(), e);
                    continue;
                }
            }
            return; // one freeze per tick
        }
    }

    /// Pressure back to normal: undo everything this episode — thaw frozen apps
    /// and release throttles.
    pub fn recover(&mut self) {
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
