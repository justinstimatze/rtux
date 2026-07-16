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
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Instant;

use nix::unistd::{Uid, User};

use crate::guard::format_bytes;
use crate::psi::PressureLevel;
use crate::ranker::AppUsage;

const DEBOUNCE_SECS: u64 = 30;

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

/// How long we will wait for a click before giving up. Comfortably longer than
/// the notification's own 12s expiry, since a user may open the drawer later.
const ACTION_WAIT_SECS: u32 = 60;

/// Send a notification carrying action buttons and block until one is clicked (or
/// it expires). Returns the clicked action key, or None. BLOCKS — callers must run
/// this off the daemon's main loop.
///
/// `notify-send --wait` used to do the waiting for us. Speaking to D-Bus directly
/// means doing it by hand: `Notify` returns immediately with an id, and the click
/// arrives later as an `ActionInvoked` *signal*. So we watch the bus for it.
///
/// **The monitor starts before the notification is sent, and that ordering is the
/// whole correctness argument.** Send-then-watch has a real race: the signal is
/// broadcast once, and a click landing between the two would be missed forever,
/// leaving the user pressing a button that does nothing — a worse failure than no
/// button, because the machine looks broken rather than quiet.
///
/// Bounded three ways so this can never wedge: `timeout` on the monitor, the
/// NotificationClosed signal, and EOF on the pipe.
pub fn notify_action(
    urgency: &str,
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
) -> Option<String> {
    eprintln!("[notify:{}] {} — {} (actionable)", urgency, summary, body);

    // Watch first — see the doc comment. `timeout` bounds the child even if we die.
    //
    // dbus-monitor rather than `gdbus monitor`, for two measured reasons. gdbus
    // monitor REQUIRES --dest and filters by sender, and its printed signal format
    // could not be verified here at all: GNOME Shell emits no NotificationClosed
    // even when handed an explicit CloseNotification, so short of a human clicking
    // a button on cue there is no way to make it print one. dbus-monitor needs no
    // --dest, catches the signal wherever it comes from, and its output format was
    // captured by execution (see the tests) rather than recalled.
    let monitor_args: Vec<String> = vec![
        ACTION_WAIT_SECS.to_string(),
        "dbus-monitor".into(),
        "--session".into(),
        format!("type='signal',path='{NOTIFY_PATH}',member='ActionInvoked'"),
    ];
    let mut monitor = session_command("timeout", &monitor_args)?
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // NOT transient — unlike the gentler notify_session notices. This is the
    // freeze notice, the one intervention that must leave a witnessable record:
    // under Do-Not-Disturb a transient notice shows no banner AND no drawer entry,
    // so the user gets zero trace of what the machine did (gh #1). Non-transient
    // persists in the drawer even when the banner is suppressed. Still normal (not
    // critical/resident) urgency — it auto-expires and never demands dismissal.
    let hints = format!("{{'urgency': <byte {}>}}", urgency_byte(urgency));
    let args = notify_call_args(summary, body, actions, &hints, 12000);
    let sent = session_command("gdbus", &args)
        .and_then(|mut c| c.stderr(Stdio::null()).output().ok());

    let id = sent
        .as_ref()
        .and_then(|o| parse_notify_id(&String::from_utf8_lossy(&o.stdout)));

    let result = match id {
        Some(id) => monitor
            .stdout
            .take()
            .and_then(|out| wait_for_action(BufReader::new(out), id)),
        // The notification never went out, so no signal is coming. Don't sit here
        // for a minute waiting for one.
        None => None,
    };

    let _ = monitor.kill();
    let _ = monitor.wait();
    result
}

/// Pull the id out of `gdbus call`'s reply, which prints `(uint32 21,)`.
fn parse_notify_id(stdout: &str) -> Option<u32> {
    let start = stdout.find("uint32")? + "uint32".len();
    let rest = &stdout[start..];
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Read dbus-monitor's output until OUR notification's button is clicked.
///
/// Parses this shape, which was captured from a live bus rather than recalled:
///
///     signal time=1784146988.717369 sender=:1.28328 -> destination=(null destination) \
///       serial=2 path=/org/freedesktop/Notifications; \
///       interface=org.freedesktop.Notifications; member=ActionInvoked
///        uint32 23
///        string "resume"
///
/// A header line, then one indented line per argument. ActionInvoked's body is
/// (uint32 id, string key), so the id and the key arrive on separate lines and
/// have to be stitched — hence the little state machine.
///
/// Split out from `notify_action` so it can be tested without a bus, a session, or
/// a human clicking a button on cue.
fn wait_for_action<R: BufRead>(reader: R, id: u32) -> Option<String> {
    let mut in_our_action = false;
    let mut id_matched = false;
    for line in reader.lines().map_while(Result::ok) {
        let t = line.trim();
        // A new message resets the machine. dbus-monitor interleaves method calls
        // and returns with signals, and their argument lines look identical to a
        // signal's — without this reset, a `string` line from some other message
        // could be read as our action key.
        if t.starts_with("signal ")
            || t.starts_with("method call ")
            || t.starts_with("method return ")
            || t.starts_with("error ")
        {
            in_our_action = t.starts_with("signal ") && t.contains("member=ActionInvoked");
            id_matched = false;
            continue;
        }
        if !in_our_action {
            continue;
        }
        if let Some(n) = t.strip_prefix("uint32 ") {
            // Only OUR notification. The bus carries every app's ActionInvoked, and
            // acting on a browser's id would resume a cgroup the user never
            // touched a button for.
            id_matched = n.trim().parse::<u32>().ok() == Some(id);
            continue;
        }
        if let Some(rest) = t.strip_prefix("string ") {
            if id_matched {
                return Some(rest.trim().trim_matches('"').to_string());
            }
            in_our_action = false;
        }
    }
    None
}

pub struct Notifier {
    last_notification: Option<Instant>,
}

impl Notifier {
    pub fn new() -> Self {
        Self { last_notification: None }
    }

    pub fn maybe_notify(&mut self, level: PressureLevel, top_apps: &[AppUsage]) {
        if level == PressureLevel::Normal {
            return;
        }

        // Debounce
        if let Some(last) = self.last_notification {
            if last.elapsed().as_secs() < DEBOUNCE_SECS {
                return;
            }
        }

        let (urgency, summary, body) = match level {
            PressureLevel::Elevated => {
                let body = format_app_list(top_apps, 3);
                ("normal", "Memory pressure rising", body)
            }
            PressureLevel::Critical => {
                let body = format!(
                    "System under heavy memory pressure.\n{}",
                    format_app_list(top_apps, 3)
                );
                // normal (not critical) urgency so it auto-expires — see notify_session.
                ("normal", "Memory pressure critical", body)
            }
            PressureLevel::Normal => unreachable!(),
        };

        notify_session(urgency, summary, &body);
        self.last_notification = Some(Instant::now());
    }
}

fn format_app_list(apps: &[AppUsage], max: usize) -> String {
    apps.iter()
        .take(max)
        .map(|a| format!("{}: {}", a.name, format_bytes(a.memory_bytes)))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VERBATIM dbus-monitor output from rukh's live session bus, 2026-07-15:
    /// GNOME Shell (sender :1.24) emitting ActionInvoked because a human actually
    /// clicked "Resume now" on a real notification. Not reconstructed from the
    /// spec, not recalled, not synthesised by `gdbus emit`.
    ///
    /// The leading NameAcquired/NameLost lines are part of the fixture on purpose.
    /// They are real, they arrive despite a match rule naming member='ActionInvoked'
    /// (dbus-monitor always gets its own name signals), and they are `signal` lines
    /// carrying a bare `string` body with NO uint32 — precisely the shape that
    /// would fool a parser without the per-message reset. They were not predicted;
    /// they turned up in the capture.
    ///
    /// An earlier draft of this parser targeted `gdbus monitor` and was written
    /// from memory of GLib's source, because that format could not be captured at
    /// all: gdbus monitor demands --dest and filters by sender, and GNOME emits no
    /// NotificationClosed even when handed an explicit CloseNotification.
    const REAL_ACTION_INVOKED: &str = r#"signal time=1784175665.893029 sender=org.freedesktop.DBus -> destination=:1.30681 serial=2 path=/org/freedesktop/DBus; interface=org.freedesktop.DBus; member=NameAcquired
   string ":1.30681"
signal time=1784175665.893081 sender=org.freedesktop.DBus -> destination=:1.30681 serial=4 path=/org/freedesktop/DBus; interface=org.freedesktop.DBus; member=NameLost
   string ":1.30681"
signal time=1784175669.752375 sender=:1.24 -> destination=(null destination) serial=8622 path=/org/freedesktop/Notifications; interface=org.freedesktop.Notifications; member=ActionInvoked
   uint32 30
   string "resume"
"#;

    #[test]
    fn reads_a_real_action_invoked_off_the_bus() {
        assert_eq!(wait_for_action(REAL_ACTION_INVOKED.as_bytes(), 30), Some("resume".into()));
    }

    /// The bus carries every app's ActionInvoked. Acting on someone else's would
    /// thaw a cgroup the user never touched a button for.
    #[test]
    fn ignores_another_apps_notification() {
        assert_eq!(wait_for_action(REAL_ACTION_INVOKED.as_bytes(), 31), None);
    }

    /// 30 must not match a line about id 301.
    #[test]
    fn does_not_prefix_match_ids() {
        let other = REAL_ACTION_INVOKED.replace("uint32 30", "uint32 301");
        assert_eq!(wait_for_action(other.as_bytes(), 30), None);
    }

    /// The bus's own name signals are `signal` lines with a bare `string` body and
    /// no uint32. Reading one as a click would thaw a cgroup on a bus housekeeping
    /// message. Real lines, from the capture above.
    #[test]
    fn the_buses_own_name_signals_are_not_clicks() {
        let housekeeping: String =
            REAL_ACTION_INVOKED.lines().take(4).collect::<Vec<_>>().join("\n");
        assert_eq!(wait_for_action(housekeeping.as_bytes(), 30), None);
    }

    /// dbus-monitor interleaves method traffic with signals, and argument lines
    /// look identical across message types. Without the per-message reset, this
    /// `string "resume"` — which belongs to a method call, not our signal — would
    /// be returned as a click the user never made.
    #[test]
    fn a_method_calls_arguments_are_not_mistaken_for_our_click() {
        let noise = format!(
            "method call time=1784146988.1 sender=:1.99 -> destination=org.foo serial=7 \
             path=/org/freedesktop/Notifications; interface=org.foo; member=Whatever\n   \
             uint32 23\n   string \"resume\"\n"
        );
        assert_eq!(wait_for_action(noise.as_bytes(), 23), None);
    }

    #[test]
    fn a_silent_bus_yields_no_click() {
        let header = "Monitoring signals from all objects\n";
        assert_eq!(wait_for_action(header.as_bytes(), 23), None);
    }

    /// gdbus prints `(uint32 26,)`. Skipping past the literal "uint32" matters:
    /// a naive scan for the first digits finds the 32 IN "uint32" and returns it
    /// as the id — which is exactly what a shell probe did while verifying this.
    #[test]
    fn parses_the_id_without_tripping_on_the_word_uint32() {
        assert_eq!(parse_notify_id("(uint32 26,)\n"), Some(26));
        assert_eq!(parse_notify_id("(uint32 5,)\n"), Some(5));
        assert_eq!(parse_notify_id(""), None);
    }

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
