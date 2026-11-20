//! Who has reached the control socket lately, and whether each desktop user can.
//!
//! Both clients swallow socket errors by design (a shell extension must not spam
//! the log; a prompt hook must not break a session), so a broken round trip is
//! silent at both ends. On 2026-08-07 the socket sat root:root for 26 hours with
//! every client getting EACCES and the daemon looking healthy.
//!
//! Two answers, neither of which infers an outage from silence (a user who walks
//! away also goes silent: 5 prompt gaps over an hour in one measured week):
//!
//! - `record`/`snapshot`: when each (uid, client) last connected, as the kernel
//!   reports the peer. Shown in `ctl list`. Legibility only, never an alert.
//! - `GroupCheck`: whether each graphical user's compositor holds the socket's
//!   group. A process gets its supplementary groups at login, so a user added to
//!   `pressured` who hasn't logged out since cannot connect at all. That is a
//!   fact the daemon can read, so it is the one condition that notifies.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::Instant;

use crate::ipc::SOCKET_PATH;
use crate::notify;

static SEEN: Mutex<Vec<(u32, &'static str, Instant)>> = Mutex::new(Vec::new());

/// Note that `uid` just reached the socket as client `kind`.
pub fn record(uid: u32, kind: &'static str) {
    let Ok(mut seen) = SEEN.lock() else { return };
    let now = Instant::now();
    match seen.iter_mut().find(|(u, k, _)| *u == uid && *k == kind) {
        Some(entry) => entry.2 = now,
        // Bounded by (users x client kinds); a handful of rows.
        None => seen.push((uid, kind, now)),
    }
}

/// (uid, client kind, seconds since it last connected), by uid then kind.
pub fn snapshot() -> Vec<(u32, &'static str, u64)> {
    let Ok(seen) = SEEN.lock() else { return Vec::new() };
    let mut out: Vec<_> = seen.iter().map(|(u, k, t)| (*u, *k, t.elapsed().as_secs())).collect();
    out.sort();
    out
}

/// Does a `/proc/PID/status` `Groups:` line include `gid`?
fn groups_line_has(line: &str, gid: u32) -> bool {
    line.split_whitespace().skip(1).any(|g| g.parse::<u32>().ok() == Some(gid))
}

/// The `Groups:` line of `uid`'s gnome-shell, the process the extension runs
/// in. None when that user has no gnome-shell (a tty or ssh session), which is
/// not a failure: there is no extension to be locked out.
fn shell_groups_line(uid: u32) -> Option<String> {
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else { continue };
        let mut name_ok = false;
        let mut uid_ok = false;
        let mut groups = None;
        for line in status.lines() {
            if let Some(n) = line.strip_prefix("Name:") {
                name_ok = n.trim() == "gnome-shell";
            } else if let Some(u) = line.strip_prefix("Uid:") {
                uid_ok = u.split_whitespace().next().and_then(|s| s.parse::<u32>().ok()) == Some(uid);
            } else if line.starts_with("Groups:") {
                groups = Some(line.to_string());
            }
        }
        if name_ok && uid_ok {
            return groups;
        }
    }
    None
}

/// Re-run every 30s from the reconcile loop. Logs on each change of state and
/// sends at most one desktop notification per user per daemon run.
#[derive(Default)]
pub struct GroupCheck {
    missing: HashSet<u32>,
    notified: HashSet<u32>,
}

impl GroupCheck {
    pub fn tick(&mut self) {
        let Ok(meta) = fs::metadata(SOCKET_PATH) else { return };
        let gid = meta.gid();
        for uid in notify::graphical_uids() {
            let Some(line) = shell_groups_line(uid) else { continue };
            if groups_line_has(&line, gid) {
                if self.missing.remove(&uid) {
                    eprintln!("ipc: uid {uid}'s desktop can reach {SOCKET_PATH} again");
                }
                continue;
            }
            if !self.missing.insert(uid) {
                continue; // already reported
            }
            eprintln!(
                "ipc: WARNING uid {uid}'s gnome-shell lacks group {gid}, so it cannot open \
                 {SOCKET_PATH}: focus reports and session touches from that desktop are \
                 refused. The session predates its group membership; log out and back in."
            );
            if self.notified.insert(uid) {
                notify::notify_user(
                    uid,
                    "normal",
                    "rtux can't see which window you're using",
                    "Your session started before you were added to the 'pressured' group, so \
                     rtux may pause the app you're in when memory runs low. Log out and back \
                     in to fix it.",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_groups_line() {
        let line = "Groups:\t4 20 24 27 991 1000 ";
        assert!(groups_line_has(line, 991));
        assert!(!groups_line_has(line, 99));
        // "Groups:" itself and partial numbers never match.
        assert!(!groups_line_has("Groups:\t9910", 991));
        assert!(!groups_line_has("Groups:", 0));
    }

    #[test]
    fn last_seen_keeps_one_row_per_client() {
        record(4242, "touch");
        record(4242, "touch");
        record(4242, "extension");
        let rows: Vec<_> = snapshot().into_iter().filter(|r| r.0 == 4242).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.2 < 5));
    }
}
