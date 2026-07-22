//! The classifier: the fast tier of the QoS controller.
//!
//! Every workload on the machine belongs to a class, ordered by its claim on the
//! box. Assigning that class is a single question — "how much does the user need
//! this right now?" — and it used to be answered in scattered boolean form:
//! `hard_exempt` here, `spared_now` there, `denied` ORing them together in the
//! mitigator while `ipc` re-derived the same facts a slightly different way. Two
//! callers asking one question two ways is exactly the confusion behind the July
//! HUD bug (a session the daemon froze while the HUD tagged it "protected"). This
//! module is the one place the question is answered, so everything agrees.
//!
//! `classify()` is pure and cheap — the daemon's hot path must never block — over
//! inputs an `Observation` has already resolved. The impure part (reading focus,
//! recent keystrokes) lives in the observers below and in the `observe` helper.

use std::path::Path;

use crate::cgroup;

/// A workload's class, ordered by claim on the machine. The ordering is the point:
/// under pressure, Idle is squeezed before Active, and Guaranteed/Focused are never
/// evicted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The spine — compositor, input method, audio, WM, the daemon itself. Hard
    /// `memory.min`, top `cpu.weight`, never evicted. Structural, from the name
    /// table below.
    Guaranteed,
    /// The workload the user is interacting with right now — the focused window's
    /// cgroup, or a terminal a human typed in within the last few seconds. Spared
    /// from eviction *while focused*; a legitimate target once attention moves on.
    /// Android's `top-app`, inferred from focus instead of declared.
    Focused,
    /// An app doing work but not focused. Under the ceiling, throttled and frozen
    /// under pressure, ordered worst-first. The default for anything in `app.slice`
    /// that isn't spine and isn't attended.
    Active,
    /// Swapped-out background sessions and unattended services — the source of
    /// reclaimed headroom, squeezed first. Not yet distinguished from Active by the
    /// fast tier (that needs the idle/fault signal, a later phase); reserved here so
    /// the class model is whole.
    #[allow(dead_code)] // constructed once the idle/fault signal lands (a later phase)
    Idle,
}

impl Class {
    /// The eviction effector must never freeze or kill these. Guaranteed is
    /// structural; Focused is momentary — the same cgroup stops being protected the
    /// moment the user looks away, which is the entire point of inferring it.
    pub fn protected_from_eviction(self) -> bool {
        matches!(self, Class::Guaranteed | Class::Focused)
    }
}

/// Hard-exempt cgroups: the spine. Freezing OR killing these breaks the session,
/// the display, or the daemon itself. The session spine + system-critical services
/// + PID 1 + the protector. Matched as substrings against both the raw cgroup dir
/// name and the humanized app name.
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
/// the foreground (or descend from it) — see `classify`. A BACKGROUND terminal
/// session (an idle shell, a build, a Claude session you're not looking at) is a
/// legitimate target: that's where this machine's pressure actually comes from,
/// and blanket-exempting it is why rtux used to sit helpless while the session
/// climbed to a global-OOM crash. Still refused for user-initiated HUD actions
/// (`never_freeze` stays a union), which is the conservative default there.
const TERMINAL_NAMES: &[&str] = &[
    "vte-spawn", "gnome-terminal", "konsole", "kitty", "alacritty",
    "xterm", "tmux", "screen", "terminator",
];

fn matches_any(name: &str, raw_dir_name: &str, list: &[&str]) -> bool {
    list.iter().any(|d| raw_dir_name.contains(d) || name.contains(d))
}

/// Structural spine membership: true when this cgroup is Guaranteed. The
/// spine/system/daemon must never be pinned, boosted, or relaxed as if it were an
/// ordinary focused app, nor ever frozen or killed. Terminals are NOT hard-exempt:
/// a focused terminal is a legitimate foreground to favour.
pub fn hard_exempt(name: &str, raw_dir_name: &str) -> bool {
    matches_any(name, raw_dir_name, HARD_EXEMPT_NAMES)
}

/// True if this cgroup must never be frozen via the IPC/HUD path. The hard-exempt
/// spine *and* every terminal — the conservative default for user-initiated
/// actions. Distinct from the class model: the auto-mitigator uses `classify` (which
/// freezes background terminals happily), while a client's explicit `ctl freeze` is
/// refused on any terminal because we can't tell from the outside that it's idle.
pub fn never_freeze(name: &str, raw_dir_name: &str) -> bool {
    hard_exempt(name, raw_dir_name) || matches_any(name, raw_dir_name, TERMINAL_NAMES)
}

/// The resolved, cheap inputs the fast tier classifies over. The impure work of
/// reading focus and recent keystrokes happens once, in `observe`; `classify`
/// itself only reads these fields, so it stays pure and microsecond-fast.
pub struct Observation<'a> {
    /// Humanized app name (`cgroup_to_app_name`'s output). NEVER a display label:
    /// the name tables match by substring and a label like "claude · rtux" would
    /// hard-exempt itself on the "rtux" fragment. See the collision test.
    pub name: &'a str,
    /// Raw cgroup directory basename.
    pub raw_dir_name: &'a str,
    /// The foreground window's cgroup, or one hosting a process that descends from
    /// it (the terminal you're in and its tabs).
    pub is_focused: bool,
    /// Reported a keystroke recently (see `ipc::LIVE`) — the only focus signal that
    /// survives tmux, where a pane descends from the server, not the focused window.
    pub touched_recently: bool,
}

/// The fast tier: pure, every tick, microseconds. Guaranteed is structural (the
/// spine table); Focused is the momentary attention overlay; everything else in
/// `app.slice` is Active. Idle is not yet split out here (it needs the idle/fault
/// signal — a later phase), so an unattended background app currently classifies as
/// Active, which is conservative: it stays a legitimate but lower-priority target.
pub fn classify(obs: &Observation) -> Class {
    if hard_exempt(obs.name, obs.raw_dir_name) {
        Class::Guaranteed
    } else if obs.is_focused || obs.touched_recently {
        Class::Focused
    } else {
        Class::Active
    }
}

/// Resolve an `Observation` for a live cgroup — the impure "observe" step that
/// reads focus and recent-touch, then hands `classify` something pure to decide on.
pub fn observe<'a>(name: &'a str, raw_dir_name: &'a str, path: &Path) -> Observation<'a> {
    Observation {
        name,
        raw_dir_name,
        is_focused: is_foreground_related(path),
        touched_recently: crate::ipc::touched_recently(path),
    }
}

/// True if the classifier would spare this cgroup **right now** because the user is
/// demonstrably using it: it's Focused. Dynamic and momentary, unlike the spine —
/// this same cgroup is a legitimate target thirty seconds after you stop typing in
/// it, which is the entire point. Exposed so the HUD can say *why* something won't
/// be paused ("not right now, you're using it") instead of implying it never will.
pub fn spared_now(path: &Path) -> bool {
    is_foreground_related(path) || crate::ipc::touched_recently(path)
}

/// True if `path` is the foreground window's cgroup, or hosts any process that
/// descends from the foreground window's pid — the terminal the user is in and all
/// of its tabs. Returns false when nothing has reported focus yet (fail-open:
/// better to act than to freeze on ambiguity under real pressure — the Guaranteed
/// spine is still protected regardless).
pub fn is_foreground_related(path: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn obs<'a>(name: &'a str, raw: &'a str, is_focused: bool, touched: bool) -> Observation<'a> {
        Observation { name, raw_dir_name: raw, is_focused, touched_recently: touched }
    }

    /// The display bug, pinned. The daemon's eviction path asks `classify`; a
    /// background Claude session in a tmux-spawn scope must classify as Active
    /// (freezable), not read as protected. `never_freeze` (the client path) still
    /// refuses it — a deliberately different, more conservative question.
    #[test]
    fn a_background_claude_session_is_active_even_though_a_client_may_not_freeze_it() {
        let raw = "tmux-spawn-0b244635-6078-4edf-8f7d-3075ad3fd91f.scope";
        let name = "Terminal (child)";
        // Unattended → Active → the daemon WILL pause it under pressure.
        assert_eq!(classify(&obs(name, raw, false, false)), Class::Active);
        assert!(!classify(&obs(name, raw, false, false)).protected_from_eviction());
        // What a CLIENT may do — deliberately conservative, and correctly unchanged.
        assert!(never_freeze(name, raw), "ctl freeze on a terminal stays refused");
    }

    /// The same scope, once the user is typing in it, is Focused and spared.
    #[test]
    fn a_touched_terminal_is_focused_and_spared() {
        let raw = "tmux-spawn-0b244635-6078-4edf-8f7d-3075ad3fd91f.scope";
        let name = "Terminal (child)";
        assert_eq!(classify(&obs(name, raw, false, true)), Class::Focused);
        assert_eq!(classify(&obs(name, raw, true, false)), Class::Focused);
        assert!(classify(&obs(name, raw, true, false)).protected_from_eviction());
    }

    /// **Never pass a display label to the classifier.** The name tables match by
    /// SUBSTRING against a list that includes the protector's own names, so the
    /// pretty label for a session working on this very repo — "claude · rtux" —
    /// contains "rtux" and would hard-exempt itself. Both real callers pass
    /// `cgroup_to_app_name`'s output ("Terminal (child)"); the display label is
    /// computed afterwards, for humans only. This trap is one keystroke away and
    /// completely silent.
    #[test]
    fn a_display_label_must_never_reach_the_classifier() {
        let raw = "tmux-spawn-0b244635-6078-4edf-8f7d-3075ad3fd91f.scope";
        assert!(hard_exempt("claude · rtux", raw));
        assert!(hard_exempt("claude · pressured", raw));
        assert_eq!(classify(&obs("claude · rtux", raw, false, false)), Class::Guaranteed);
        // …whereas the name the code really passes is safe.
        assert!(!hard_exempt("Terminal (child)", raw));
    }

    /// The spine is Guaranteed under the classifier and refused under `never_freeze`
    /// — the extraction must not have widened what the eviction effector will touch.
    #[test]
    fn the_spine_is_guaranteed_and_never_frozen() {
        for (name, raw) in [
            ("org.gnome.Shell", "org.gnome.Shell@ubuntu.service"),
            ("pipewire", "pipewire.service"),
            ("wireplumber", "wireplumber.service"),
            ("dbus", "dbus.service"),
        ] {
            assert_eq!(
                classify(&obs(name, raw, false, false)),
                Class::Guaranteed,
                "{name} must classify as spine"
            );
            // Even if the spine somehow reported focus, it stays Guaranteed.
            assert_eq!(classify(&obs(name, raw, true, true)), Class::Guaranteed);
            assert!(never_freeze(name, raw), "{name} must never be client-frozen");
        }
    }

    /// Firefox is an ordinary app: Active when unattended, Focused when in front,
    /// and never terminal-exempt under `never_freeze`.
    #[test]
    fn an_ordinary_app_is_active_or_focused_by_attention() {
        let raw = "snap.firefox.firefox-247a1653-e08f-4045-9845-9cc6ac38b2f6.scope";
        assert_eq!(classify(&obs("Firefox", raw, false, false)), Class::Active);
        assert_eq!(classify(&obs("Firefox", raw, true, false)), Class::Focused);
        assert!(!hard_exempt("Firefox", raw));
        assert!(!never_freeze("Firefox", raw));
    }
}
