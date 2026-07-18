//! Desktop notifications, spoken to D-Bus directly rather than through
//! `notify-send`.
//!
//! # Why not notify-send
//!
//! Because AppArmor denies it, and the denial is invisible unless you go looking.
//! Measured 2026-07-15 — the kernel's own account:
//!
//!     apparmor="DENIED" operation="connect" info="Failed name lookup - disconnected path"
//!     error=-13 profile="notify-send" name="run/user/1000/bus" fsuid=1000 ouid=1000
//!
//! Read `name="run/user/1000/bus"` closely: **there is no leading slash.** Ubuntu
//! ships an AppArmor profile for notify-send permitting `/run/user/*/bus`. The
//! daemon runs with `ProtectSystem=full`, so systemd builds it a mount namespace
//! (via pivot_root/MS_MOVE); inside one, AppArmor cannot resolve that path back to
//! the root namespace and reports it "disconnected", handing the matcher a
//! *relative* name that the profile's absolute rule cannot match. Denied. EACCES.
//! `notify-send` prints "Could not connect: Permission denied" — which reads
//! exactly like a file-permission problem and is nothing of the kind.
//!
//! Everything you would suspect first is innocent, and each was measured to be:
//! the socket is `srw-rw-rw-`; `setuid(1000)` succeeds; `/run/user/1000` (dev
//! 0:113, mode=700 uid=1000) IS present in the daemon's namespace per
//! /proc/PID/mountinfo. `ProtectSystem` looked guilty only because a first pass
//! dropped directives one at a time from a set where it was the ONLY one that
//! creates a mount namespace — `PrivateTmp` and `ProtectHome` fail identically.
//! The namespace is the cause; ProtectSystem was a proxy for it.
//!
//! # Why gdbus fixes it without giving up any hardening
//!
//! **AppArmor attaches a profile on `exec`.** The daemon itself is `unconfined`,
//! and of the binaries that can reach the bus, only notify-send carries a profile
//! — gdbus, dbus-send and busctl are all unconfined. So calling
//! `org.freedesktop.Notifications.Notify` through gdbus is mediated by nothing and
//! the disconnected-path problem never arises. Verified under the unit's *full*
//! confinement, not a reduced one: notify-send FAILS, gdbus WORKS, and gdbus draws
//! zero denials.
//!
//! The alternative was deleting `ProtectSystem=full` from a root daemon that
//! writes cgroups and other processes' oom_score_adj — trading real hardening to
//! avoid understanding a bug.
//!
//! # The journal is the durable record
//!
//! Every function here `eprintln!`s its text BEFORE attempting delivery, so the
//! journal keeps the record even when the desktop never shows it. That ordering is
//! load-bearing: this bug meant rtux froze five apps during the 2026-07-15
//! incident and told the user about none of them, and only the journal knew.

use std::fs;
use std::process::{Command, Stdio};

use nix::unistd::{Uid, User};

const NOTIFY_DEST: &str = "org.freedesktop.Notifications";
const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";

/// Find the graphical user's uid by looking for a live session bus under /run/user.
/// Picks the lowest uid >= 1000 that has a D-Bus socket.
pub fn graphical_uid() -> Option<u32> {
    let mut best: Option<u32> = None;
    for entry in fs::read_dir("/run/user").ok()?.flatten() {
        if let Ok(uid) = entry.file_name().to_string_lossy().parse::<u32>() {
            if uid >= 1000 && entry.path().join("bus").exists() {
                best = Some(best.map_or(uid, |b| b.min(uid)));
            }
        }
    }
    best
}

/// Build a command that runs `program` inside the graphical user's session.
///
/// As root: `runuser` into the session, with the bus env injected *inside* the
/// target via `env` rather than `Command::env` — runuser opens a PAM session that
/// resets the environment, wiping anything set on our side. Without those two vars
/// the client cannot find the user's bus at all.
///
/// As the user: run it directly; the env is already ours.
///
/// None means there is no graphical session to talk to, which is a normal state
/// (rtux starts at boot, before login) and not an error.
fn session_command(program: &str, args: &[String]) -> Option<Command> {
    if !Uid::effective().is_root() {
        let mut cmd = Command::new(program);
        cmd.args(args);
        return Some(cmd);
    }
    let uid = graphical_uid()?;
    let user = User::from_uid(Uid::from_raw(uid)).ok().flatten()?;
    let runtime = format!("/run/user/{uid}");
    let bus = format!("unix:path={runtime}/bus");
    let mut cmd = Command::new("runuser");
    cmd.args(["-u", &user.name, "--", "env"])
        .arg(format!("XDG_RUNTIME_DIR={runtime}"))
        .arg(format!("DBUS_SESSION_BUS_ADDRESS={bus}"))
        .arg(program)
        .args(args);
    Some(cmd)
}

/// Quote a string as GVariant text, which is what `gdbus call` parses each
/// positional argument as.
///
/// Not cosmetic. These strings carry app names derived from cgroup leaves, and
/// systemd escapes those — `app-gnome-google\x2dchrome-2522777.scope` arrives with
/// a literal backslash. Handing that to gdbus unquoted invites it to parse the
/// argument as some other GVariant type, or to fail outright, and a notification
/// that silently doesn't render is this whole module's existing bug wearing a new
/// hat. Quote and escape, always.
fn gvariant_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// urgency name -> the byte the spec wants (0 low, 1 normal, 2 critical).
fn urgency_byte(urgency: &str) -> u8 {
    match urgency {
        "low" => 0,
        "critical" => 2,
        _ => 1,
    }
}

/// Arguments for `gdbus call ... org.freedesktop.Notifications.Notify`.
///
/// Signature is (susssasa{sv}i): app_name, replaces_id, app_icon, summary, body,
/// actions, hints, expire_timeout. `actions` is a FLAT array alternating key and
/// label — not pairs — which is the spec's shape and easy to get wrong.
fn notify_call_args(
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
    hints: &str,
    expire_ms: i32,
) -> Vec<String> {
    let actions_arg = if actions.is_empty() {
        "@as []".to_string()
    } else {
        let items: Vec<String> = actions
            .iter()
            .flat_map(|(key, label)| [gvariant_str(key), gvariant_str(label)])
            .collect();
        format!("[{}]", items.join(", "))
    };
    vec![
        "call".into(),
        "--session".into(),
        "--dest".into(),
        NOTIFY_DEST.into(),
        "--object-path".into(),
        NOTIFY_PATH.into(),
        "--method".into(),
        format!("{NOTIFY_DEST}.Notify"),
        gvariant_str("pressured"),
        "0".into(),
        gvariant_str("dialog-warning"),
        gvariant_str(summary),
        gvariant_str(body),
        actions_arg,
        hints.into(),
        expire_ms.to_string(),
    ]
}

/// Send a desktop notification, and always log the same text to the journal first.
///
/// rtux never demands acknowledgement: transient + short expiry so events fade on
/// their own. (Critical urgency is resident on GNOME and would have to be
/// dismissed by hand — so callers use normal urgency.)
pub fn notify_session(urgency: &str, summary: &str, body: &str) {
    eprintln!("[notify:{}] {} — {}", urgency, summary, body);

    let hints = format!("{{'transient': <true>, 'urgency': <byte {}>}}", urgency_byte(urgency));
    let args = notify_call_args(summary, body, &[], &hints, 6000);
    let Some(mut cmd) = session_command("gdbus", &args) else { return };
    // Fire and forget, but keep the child's chatter out of the journal: a failure
    // here is already recorded by the eprintln above, and gdbus's own stderr would
    // just be a second, less legible copy.
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// App names come from systemd cgroup leaves, which are escaped:
    /// `app-gnome-google\x2dchrome-2522777.scope` carries a literal backslash. An
    /// unquoted arg invites gdbus to parse it as some other GVariant type.
    #[test]
    fn quotes_strings_that_would_otherwise_break_the_gvariant_parser() {
        assert_eq!(gvariant_str("plain"), "'plain'");
        assert_eq!(gvariant_str(r"google\x2dchrome"), r"'google\\x2dchrome'");
        assert_eq!(gvariant_str("it's"), r"'it\'s'");
        // A body starting with '[' must not be read as an array.
        assert_eq!(gvariant_str("[not an array]"), "'[not an array]'");
    }

    /// The spec wants actions as a FLAT array alternating key and label, not pairs.
    #[test]
    fn actions_serialise_flat_not_as_pairs() {
        let args = notify_call_args("s", "b", &[("resume", "Resume now")], "{}", 6000);
        assert!(
            args.iter().any(|a| a == "['resume', 'Resume now']"),
            "actions must be a flat as, got: {args:?}"
        );
    }

    #[test]
    fn no_actions_is_a_typed_empty_array() {
        // A bare `[]` is ambiguous to gdbus; `@as []` names the type.
        let args = notify_call_args("s", "b", &[], "{}", 6000);
        assert!(args.iter().any(|a| a == "@as []"));
    }

    #[test]
    fn urgency_maps_to_the_bytes_the_spec_defines() {
        assert_eq!(urgency_byte("low"), 0);
        assert_eq!(urgency_byte("normal"), 1);
        assert_eq!(urgency_byte("critical"), 2);
        // Unknown urgency must land on normal, never on resident-critical.
        assert_eq!(urgency_byte("nonsense"), 1);
    }
}
