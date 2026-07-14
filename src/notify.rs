use std::fs;
use std::process::Command;
use std::time::Instant;

use nix::unistd::{Uid, User};

use crate::guard::format_bytes;
use crate::psi::PressureLevel;
use crate::ranker::AppUsage;

const DEBOUNCE_SECS: u64 = 30;

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

/// Send a desktop notification, and always log the same text to stderr (journal).
/// Works whether the daemon runs as the user (direct notify-send) or as root
/// (drops into the graphical session via runuser with the right bus env).
pub fn notify_session(urgency: &str, summary: &str, body: &str) {
    eprintln!("[notify:{}] {} — {}", urgency, summary, body);

    // rtux never demands acknowledgement: transient + short expiry so events
    // fade on their own. (Critical-urgency notifications are resident on GNOME
    // and would have to be dismissed by hand — so callers use normal urgency.)
    let args = [
        "--urgency", urgency,
        "--expire-time", "6000",
        "--hint", "int:transient:1",
        "--app-name", "pressured",
        "--icon", "dialog-warning",
        summary, body,
    ];

    if Uid::effective().is_root() {
        let Some(uid) = graphical_uid() else { return };
        let Ok(Some(user)) = User::from_uid(Uid::from_raw(uid)) else { return };
        let runtime = format!("/run/user/{}", uid);
        let bus = format!("unix:path={}/bus", runtime);
        // Inject the session-bus env *inside* the target via `env`, not via
        // Command::env: runuser runs a PAM session that resets the environment,
        // wiping vars we set on our side. Without them notify-send can't find the
        // user's bus and dies with "Could not connect: Permission denied" — which
        // is exactly why the crash-time notifications never reached the screen.
        let mut cmd = Command::new("runuser");
        cmd.args(["-u", &user.name, "--", "env"])
            .arg(format!("XDG_RUNTIME_DIR={}", runtime))
            .arg(format!("DBUS_SESSION_BUS_ADDRESS={}", bus))
            .arg("notify-send")
            .args(args);
        let _ = cmd.spawn();
    } else {
        let _ = Command::new("notify-send").args(args).spawn();
    }
}

/// Send a notification carrying action buttons and block until one is clicked
/// (or it expires). Returns the clicked action key, or None. BLOCKS — callers
/// must run this off the daemon's main loop (spawn a thread). Wrapped in
/// `timeout` so a never-touched notification can't wedge the waiter forever.
pub fn notify_action(
    urgency: &str,
    summary: &str,
    body: &str,
    actions: &[(&str, &str)],
) -> Option<String> {
    let mut ns: Vec<String> = vec![
        "--wait".into(),
        "--urgency".into(),
        urgency.into(),
        "--expire-time".into(),
        "12000".into(),
        // NOT transient — unlike the gentler notify_session notices. This is the
        // freeze notice, the one intervention that must leave a witnessable
        // record: under Do-Not-Disturb a transient notice shows no banner AND no
        // drawer entry, so the user gets zero trace of what the machine did (gh
        // #1). Non-transient persists in the notification drawer even when the
        // banner is suppressed. Still normal (not critical/resident) urgency — the
        // banner auto-expires and it never demands manual dismissal.
        "--app-name".into(),
        "pressured".into(),
        "--icon".into(),
        "dialog-warning".into(),
    ];
    for (key, label) in actions {
        ns.push("--action".into());
        ns.push(format!("{}={}", key, label));
    }
    ns.push(summary.into());
    ns.push(body.into());

    eprintln!("[notify:{}] {} — {} (actionable)", urgency, summary, body);

    let out = if Uid::effective().is_root() {
        let uid = graphical_uid()?;
        let user = User::from_uid(Uid::from_raw(uid)).ok().flatten()?;
        let runtime = format!("/run/user/{}", uid);
        let bus = format!("unix:path={}/bus", runtime);
        // `env` inside the target (see notify_session) so the bus vars survive
        // runuser's PAM environment reset.
        Command::new("timeout")
            .args(["60", "runuser", "-u", &user.name, "--", "env"])
            .arg(format!("XDG_RUNTIME_DIR={}", runtime))
            .arg(format!("DBUS_SESSION_BUS_ADDRESS={}", bus))
            .arg("notify-send")
            .args(&ns)
            .output()
            .ok()?
    } else {
        Command::new("timeout")
            .arg("60")
            .arg("notify-send")
            .args(&ns)
            .output()
            .ok()?
    };
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
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
