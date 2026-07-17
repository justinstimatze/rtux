use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::{actions, cgroup, classify, guard, mitigate, notify, psi};

pub const SOCKET_PATH: &str = "/run/pressured.sock";
const LIST_MIN_BYTES: u64 = 100 * 1024 * 1024; // surface apps >= 100 MB
const CGROUP_BASE: &str = "/sys/fs/cgroup";

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    List,
    Act { action: String, id: String },
    /// The HUD asks to be pinned into the reserved control plane (kept resident +
    /// OOM-protected) so it stays operable under load. The pid in the JSON is
    /// ignored — we trust the kernel's SO_PEERCRED peer instead (see handle()).
    #[allow(dead_code)]
    PinSelf { pid: i32 },
    /// A focus tracker reports the newly-focused window's pid. The daemon pins
    /// that app resident (clamped) and relaxes the previous foreground — so the
    /// window you're in is always instant. Client-agnostic (shell ext,
    /// AT-SPI, …).
    Foreground { pid: i32 },
    /// A session reports that a human just interacted with it ("I was typed in").
    /// Carries NO pid on purpose: the caller's own cgroup is resolved from the
    /// kernel's SO_PEERCRED peer, so a client can only ever mark *itself* live and
    /// never exempt someone else's cgroup from mitigation. See LIVE.
    Touch,
    /// "Can this machine afford `want_mb` more right now?" — the admission-control
    /// query. Read-only and target-less: it names no cgroup and writes nothing, so
    /// unlike `Act` there is nothing here for `permitted_target` to guard. The only
    /// untrusted input is an integer, and it is saturated before use.
    Budget {
        #[serde(default)]
        want_mb: Option<u64>,
    },
}

#[derive(Serialize)]
struct App {
    id: String,
    name: String,
    mem_bytes: u64,
    swap_bytes: u64,
    frozen: bool,
    protected: bool,
    freezable: bool,
    /// Spared from an automatic pause *right now* because the user is using it
    /// (foreground, or a recent keystroke). Momentary — distinct from `freezable`,
    /// which is about structural eligibility and does not change minute to minute.
    spared: bool,
    flagged: Option<String>,
}

#[derive(Serialize)]
struct RecentEvent {
    ago_secs: u64,
    text: String,
}

#[derive(Serialize)]
struct ListReply {
    ok: bool,
    some_avg10: f64,
    full_avg10: f64,
    mem_used: u64,
    mem_total: u64,
    swap_used: u64,
    swap_total: u64,
    apps: Vec<App>,
    recent: Vec<RecentEvent>,
    /// Rolling PSI some.avg10 history (oldest-first) for the HUD sparkline.
    pressure_trend: Vec<f64>,
    /// Spine major faults in the last tick — is the interactive path waiting on
    /// disk *right now*. None when the meter read no spine cgroup at all, which
    /// means "unknown", NOT "zero" (see health::Sample::observed).
    spine_faults_now: Option<u64>,
    /// Worst tick in the last ~minute. Carried separately because a stall is over
    /// long before anyone opens the HUD, so the instantaneous value alone would
    /// report every past incident as "clean".
    spine_faults_peak: Option<u64>,
    /// Which spine class hurt most in the last tick, if any did.
    spine_worst: Option<String>,
    /// How many spine cgroups the last tick actually read. Zero is the alarm: it
    /// means rtux is guarding a spine it cannot find.
    spine_observed: usize,
}

/// (used, total) for RAM and swap, from /proc/meminfo (bytes).
fn mem_swap() -> (u64, u64, u64, u64) {
    let (mut mt, mut ma, mut st, mut sf) = (0u64, 0u64, 0u64, 0u64);
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let kb: u64 = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
            match k.trim() {
                "MemTotal" => mt = kb * 1024,
                "MemAvailable" => ma = kb * 1024,
                "SwapTotal" => st = kb * 1024,
                "SwapFree" => sf = kb * 1024,
                _ => {}
            }
        }
    }
    (mt.saturating_sub(ma), mt, st.saturating_sub(sf), st)
}

#[derive(Serialize)]
struct ActReply {
    ok: bool,
    msg: String,
}

#[derive(Serialize)]
struct BudgetReply {
    ok: bool,
    /// "ok" | "tight" | "full". Callers should branch on THIS, not on `ok` — `ok`
    /// only says the daemon answered, not that the answer was yes.
    verdict: String,
    reason: String,
    headroom_bytes: u64,
    ceiling_bytes: Option<u64>,
    app_current_bytes: u64,
    mem_available_bytes: u64,
    psi_some_avg10: f64,
    want_bytes: u64,
}

fn build_budget(want_mb: Option<u64>) -> BudgetReply {
    // Saturate rather than wrap: `want_mb` is untrusted, and u64::MAX MB would
    // otherwise overflow into a small number and turn a preposterous ask into an
    // "ok". Saturating means an absurd ask reads as absurd and gets refused.
    let want = want_mb.map(|mb| mb.saturating_mul(1024 * 1024));
    let b = guard::budget(want);
    BudgetReply {
        ok: true,
        verdict: b.verdict.to_string(),
        reason: b.reason,
        headroom_bytes: b.headroom,
        ceiling_bytes: b.ceiling,
        app_current_bytes: b.app_current,
        mem_available_bytes: b.mem_available,
        psi_some_avg10: b.psi_some_avg10,
        want_bytes: want.unwrap_or(0),
    }
}

/// Start the control socket in a background thread. Failures here are non-fatal:
/// the protection loop runs regardless of whether a HUD can attach.
pub fn spawn_server() {
    thread::spawn(|| {
        if let Err(e) = serve() {
            eprintln!("ipc: control socket unavailable: {}", e);
        }
    });
}

fn serve() -> std::io::Result<()> {
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH)?;

    // Let the graphical user (by primary group) connect; keep everyone else out.
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o660))?;
    if let Some(uid) = notify::graphical_uid() {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
            let _ = nix::unistd::chown(SOCKET_PATH, None, Some(user.gid));
        }
    }
    eprintln!("ipc: control socket listening at {}", SOCKET_PATH);

    for stream in listener.incoming().flatten() {
        // One thread per connection: a slow/silent client can't wedge the socket
        // for everyone, and a panic in one handler can't take the server down.
        thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = handle(stream);
            }));
        });
    }
    Ok(())
}

fn handle(stream: UnixStream) -> std::io::Result<()> {
    // A client that connects and never sends a line must not block this thread
    // forever (it no longer blocks *others*, but still shouldn't leak).
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    // Kernel-verified identity of the connecting process. Trust this, never a pid
    // a client puts in its JSON (which it can set to anything).
    let peer_pid = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
        .ok()
        .map(|c| c.pid());
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    // Bound the request: the read timeout resets on every byte, so a client that
    // drips bytes with no newline would otherwise grow `line` unboundedly and OOM
    // this (root) daemon — ironic for an OOM-prevention tool. 64 KiB is far more
    // than any real command.
    if (&mut reader).take(64 * 1024).read_line(&mut line)? == 0 {
        return Ok(());
    }
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::List) => serde_json::to_string(&build_list()),
        Ok(Request::Act { action, id }) => serde_json::to_string(&do_act(&action, &id)),
        // Ignore the client-supplied pid entirely — pin only the caller itself,
        // identified by the kernel.
        Ok(Request::PinSelf { .. }) => serde_json::to_string(&do_pin_self(peer_pid)),
        Ok(Request::Foreground { pid }) => serde_json::to_string(&do_foreground(pid)),
        // Same trust rule as PinSelf: the kernel's peer, never a client's claim.
        Ok(Request::Touch) => serde_json::to_string(&do_touch(peer_pid)),
        Ok(Request::Budget { want_mb }) => serde_json::to_string(&build_budget(want_mb)),
        Err(e) => serde_json::to_string(&ActReply {
            ok: false,
            msg: format!("bad request: {}", e),
        }),
    }
    .unwrap_or_else(|_| "{\"ok\":false,\"msg\":\"serialize error\"}".into());
    writeln!(writer, "{}", reply)
}

fn build_list() -> ListReply {
    let (some_avg10, full_avg10) = match psi::read_psi("/proc/pressure/memory") {
        Ok(r) => (r.some.avg10, r.full.map(|f| f.avg10).unwrap_or(0.0)),
        Err(_) => (0.0, 0.0),
    };
    let raw_apps = cgroup::list_apps(LIST_MIN_BYTES);

    // Flag the largest consumer rtux would *actually* pause first — same floor AND
    // the same predicate the mitigator uses, so the top-consumer marker never
    // promises a pause that won't come, nor stays silent about one that will.
    //
    // This used `never_freeze`, which is the wrong question. `never_freeze` answers
    // "may a CLIENT freeze this via ctl?" and deliberately refuses every terminal
    // (see its doc comment: "the conservative default for user-initiated actions").
    // The auto-mitigator asks a different question and answers it in `denied`, which
    // checks only `hard_exempt` plus the dynamic foreground/live spares. Measured
    // 2026-07-15: the HUD marked Firefox (1.2GB) as the top consumer while
    // claude · lexicon (2.0GB) sat tagged `critical` — and rtux froze lexicon.
    let top = raw_apps
        .iter()
        .find(|a| {
            a.has_freeze
                && a.mem >= mitigate::MIN_FREEZE_BYTES
                && !classify::hard_exempt(&a.name, &a.raw)
                && !classify::spared_now(&a.path)
        })
        .map(|a| a.path.clone());

    let apps = raw_apps
        .into_iter()
        .map(|a| {
            // "Would the auto-mitigator pause this under pressure?" — which is what
            // a HUD reader is asking, and what rtux's legibility promise is about.
            // NOT `never_freeze`: that answers "may a client freeze this via ctl?"
            // and refuses every terminal, so every Claude session in a tmux-spawn
            // scope reported false and the HUD tagged it `critical`, which reads as
            // "protected". The daemon froze those same sessions all along (measured:
            // "Froze claude · rtux (1.6GB)" against a reply calling that exact scope
            // unfreezable). A display that contradicts the daemon is the same defect
            // as the display-string gate and the cumulative-fault scar.
            let freezable = a.has_freeze && !classify::hard_exempt(&a.name, &a.raw);
            // Whether it is spared *at this moment* is a separate fact from whether
            // it is eligible at all, and collapsing the two is how the original bug
            // read as "protected" forever. Carried so the HUD can say "not right
            // now, you're using it" rather than "never".
            let spared = classify::spared_now(&a.path);
            let id = a
                .path
                .strip_prefix(CGROUP_BASE)
                .unwrap_or(&a.path)
                .to_string_lossy()
                .to_string();
            let flagged = if Some(&a.path) == top.as_ref() {
                Some("top_consumer".to_string())
            } else {
                None
            };
            // Terminal child scopes (vte-spawn / tmux-spawn) → name by their
            // session (a Claude session becomes "claude · dir") or biggest
            // process + cwd, instead of a mangled UUID scope name.
            let name = if a.raw.starts_with("vte-spawn") || a.raw.starts_with("tmux-spawn") {
                cgroup::proc_label(&a.path).unwrap_or(a.name)
            } else {
                a.name
            };
            App {
                id,
                name,
                mem_bytes: a.mem,
                swap_bytes: a.swap,
                frozen: a.frozen,
                protected: a.mem_min > 0,
                freezable,
                spared,
                flagged,
            }
        })
        .collect();

    let (mem_used, mem_total, swap_used, swap_total) = mem_swap();
    let recent = crate::events::recent()
        .into_iter()
        .map(|(ago_secs, text)| RecentEvent { ago_secs, text })
        .collect();
    let latest = crate::health::latest();
    ListReply {
        ok: true,
        some_avg10,
        full_avg10,
        mem_used,
        mem_total,
        swap_used,
        swap_total,
        apps,
        recent,
        pressure_trend: crate::trend::history(),
        spine_faults_now: (latest.observed > 0).then_some(latest.faults),
        spine_faults_peak: crate::health::peak(),
        spine_worst: latest.worst.map(|(name, _)| name),
        spine_observed: latest.observed,
    }
}

/// Pin the HUD process resident + OOM-protected. `pid` is the *kernel-reported*
/// connecting process (from SO_PEERCRED), not a client-supplied value — so a
/// client can only ever pin itself, never an arbitrary pid. The comm check stays
/// as defence-in-depth (a member could name their own process "pressured-hud",
/// but then they only pin that one process — a bounded self-only effect).
fn do_pin_self(pid: Option<i32>) -> ActReply {
    let Some(pid) = pid else {
        return ActReply {
            ok: false,
            msg: "refused: could not verify the calling process".into(),
        };
    };
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if comm != "pressured-hud" {
        return ActReply {
            ok: false,
            msg: "refused: caller is not the HUD".into(),
        };
    }
    // Keep it out of the OOM killer's sights.
    let _ = std::fs::write(format!("/proc/{}/oom_score_adj", pid), "-600");
    // Keep its pages resident so it paints instantly when summoned under load.
    if let Some(cg) = cgroup::cgroup_of_pid(pid) {
        let _ = guard::protect_cgroup(&cg);
    }
    ActReply {
        ok: true,
        msg: "HUD pinned into the reserved control plane".into(),
    }
}

// The app currently favoured as foreground (its memory.min is raised).
static FOREGROUND: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
// The focused *window's* pid (the terminal emulator, for a terminal). The
// auto-mitigator uses it to spare the active terminal and all its tabs from
// freeze/kill — their processes descend from this pid (see pid_descends_from).
static FOREGROUND_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The focused window's pid, or None if nothing has reported focus yet.
pub fn foreground_pid() -> Option<i32> {
    let p = FOREGROUND_PID.load(std::sync::atomic::Ordering::Relaxed);
    (p > 0).then_some(p)
}

/// Cgroups a human has recently interacted with, newest-first.
///
/// Why this exists: under tmux, foreground-sparing is not merely weakened — it is
/// **inoperative**. A pane's processes descend from the tmux *server*, which
/// systemd parents to `systemd --user`, so the chain from the pane never reaches
/// the focused terminal window and `pid_descends_from` reports even the pane
/// under your fingers as background. It was freezable while being typed in.
///
/// Nothing observable from outside fixes this:
///   * every tmux client descends from the *same* terminal-emulator pid (one
///     process, N tabs), so the focused tab is invisible at pid granularity;
///   * tmux counts *output* as activity, so `client_activity` ranks a chatty
///     background agent above the human (measured: it picked a busy background
///     session while the user was demonstrably typing in another).
///
/// So the session tells us instead — a UserPromptSubmit hook pings the socket the
/// moment a prompt is submitted. "A human typed here" is a fact only the session
/// has, and it is exactly the right sparing signal.
static LIVE: std::sync::Mutex<Vec<(std::path::PathBuf, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());
/// How long a touched cgroup stays spared. Long enough to cover reading/thinking
/// between prompts; short enough that a session you walked away from returns to
/// the freezable pool on its own.
const TOUCH_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Cap on the spared set. Bounds the honest case (cycling between a few panes)
/// and the pathological one (a process spamming touch to make itself
/// unfreezable): the mitigator must never be starved of candidates, or we're back
/// to the helpless climb that ended in a global-OOM session kill.
const MAX_LIVE: usize = 3;

/// Mark the CALLER's own cgroup as live. `pid` is the kernel-reported peer
/// (SO_PEERCRED), never a client-supplied value — so this can only ever spare the
/// caller's own session, which is the whole security story for this endpoint.
fn do_touch(pid: Option<i32>) -> ActReply {
    let Some(pid) = pid else {
        return ActReply {
            ok: false,
            msg: "refused: could not verify the calling process".into(),
        };
    };
    let Some(cg) = cgroup::cgroup_of_pid(pid) else {
        return ActReply {
            ok: false,
            msg: format!("no cgroup for pid {}", pid),
        };
    };
    let now = std::time::Instant::now();
    let (expired, name) = {
        let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        // Drop this cgroup's old entry and anything expired, then re-add at the
        // front so the newest touch wins the truncation below. Whatever falls out
        // (aged out, or pushed past MAX_LIVE) must be released, or the pins leak.
        let mut expired: Vec<std::path::PathBuf> = Vec::new();
        live.retain(|(p, t)| {
            let keep = p != &cg && now.duration_since(*t) < TOUCH_TTL;
            if !keep && p != &cg {
                expired.push(p.clone());
            }
            keep
        });
        live.insert(0, (cg.clone(), now));
        let keep = MAX_LIVE.min(live.len());
        for (p, _) in live.drain(keep..) {
            expired.push(p);
        }
        (expired, cgroup::proc_label(&cg).unwrap_or_else(|| "session".to_string()))
    };

    // Hand back sessions the human has moved on from.
    for p in expired {
        let _ = guard::unprotect_cgroup(&p);
    }

    // Make it RESIDENT — not merely unfrozen.
    //
    // Sparing a session from freeze was never enough, and assuming it was is why
    // typing stayed laggy through a whole day of "fixes". Measured on the session
    // the user was actively typing in: memory.min=0, memory.swap.max=max, and 43%
    // of it (369MB of an 820MB footprint) paged out — onto the DISK swapfile,
    // since zram was 100% full. Every keystroke woke claude, touched an evicted
    // page, and paid a disk fault: io PSI sat at ~35% sustained while cpu PSI was
    // ~2, so this was never the CPU contention the throttling rungs were built
    // for. rtux hardened the compositor and left the terminal — the one process
    // that has to render every character — entirely unprotected.
    //
    // protect_foreground pins the pages (memory.min), forbids swapping for
    // anything under 1/8 RAM (memory.swap.max=0), and boosts cpu.weight. That is
    // what "the window you're in is always instant" has to mean under tmux, where
    // the focused-window path can't see the pane at all.
    let _ = guard::protect_foreground(&cg);

    ActReply {
        ok: true,
        msg: format!("{} is live — pinned resident for {}s", name, TOUCH_TTL.as_secs()),
    }
}

/// True if a human interacted with this cgroup recently. Also matches when the
/// touched cgroup lives *under* `path`, so sparing a pane spares its ancestors.
pub fn touched_recently(path: &std::path::Path) -> bool {
    let now = std::time::Instant::now();
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.iter()
        .any(|(p, t)| now.duration_since(*t) < TOUCH_TTL && (p == path || p.starts_with(path)))
}

/// Pin the focused app resident and relax the previous one. Session-critical
/// cgroups (compositor/audio/etc.) are left untouched — guard already protects
/// them, and we must never clear that.
fn do_foreground(pid: i32) -> ActReply {
    // Remember the focused window's pid so the mitigator can spare the terminal
    // the user is in (and its tabs) — independent of the memory.min pinning below.
    FOREGROUND_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
    let Some(cg) = cgroup::cgroup_of_pid(pid) else {
        return ActReply { ok: false, msg: format!("no cgroup for pid {}", pid) };
    };
    let raw = cg
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = cgroup::cgroup_to_app_name(&raw);
    // Only skip the hard-exempt spine (compositor/audio/dbus/system) — guard
    // already protects those and we must not disturb them. A focused *terminal*
    // is NOT hard-exempt: it's a legitimate foreground to favour (memory pin +
    // cpu.weight boost), which matters since the user is often typing in one.
    if classify::hard_exempt(&name, &raw) {
        return ActReply { ok: true, msg: format!("{} is a protected service — left as-is", name) };
    }

    // Recover from a poisoned lock rather than panicking the handler thread.
    let mut fg = FOREGROUND.lock().unwrap_or_else(|e| e.into_inner());
    if fg.as_deref() == Some(cg.as_path()) {
        return ActReply { ok: true, msg: "unchanged".into() };
    }
    if let Some(prev) = fg.as_ref() {
        let _ = guard::unprotect_cgroup(prev);
    }
    let _ = guard::protect_foreground(&cg);
    *fg = Some(cg);
    ActReply { ok: true, msg: format!("foreground → {}", name) }
}

/// Is this resolved cgroup a permitted target for a mutating action? The socket
/// is reachable by any process running as the desktop user, so a client-supplied
/// id is untrusted. `resolve_id` already confines it to the cgroup tree, but that
/// still admits *slices*, the root, and `init.scope` — freezing/killing any of
/// which takes down the session (or the whole system). So require a concrete unit
/// and run the denylist against EVERY path component (not just the leaf, which is
/// how an ancestor slice like `user.slice` slipped past), plus a self/ancestor/
/// descendant guard so we can never target the daemon itself.
fn permitted_target(path: &std::path::Path, action: &str) -> bool {
    let leaf = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Concrete unit only — never a slice or the cgroup root.
    if !(leaf.ends_with(".scope") || leaf.ends_with(".service")) {
        return false;
    }
    // The *target* must never be a systemd manager (`user@N.service` / the root
    // `init.scope`): those hold the whole session / PID 1, and freezing one is an
    // unrecoverable lockup. Note we check the LEAF here — `user@N.service` is a
    // legitimate *ancestor* of every user app, so it must not be denied wholesale.
    if leaf.starts_with("user@") {
        return false;
    }
    // Session-critical check against every component, so an ancestor unit
    // (init.scope) or a critical parent can't be reached through a child id.
    for comp in path.components() {
        let c = comp.as_os_str().to_string_lossy();
        if classify::never_freeze(&c, &c) {
            return false;
        }
    }
    // Never the daemon itself, an ancestor of it, or a descendant of it.
    if let Some(self_cg) = cgroup::self_cgroup() {
        if self_cg.starts_with(path) || path.starts_with(&self_cg) {
            return false;
        }
    }
    // Irreversible kill is confined to the user's OWN apps (under app.slice).
    // Reversible actions (freeze/cap/protect) may touch a system service like
    // ollama, but an unprivileged client must never be able to *kill* a root
    // service (data loss / privilege escalation) through this root daemon.
    if action == "kill"
        && !path
            .components()
            .any(|c| c.as_os_str() == "app.slice")
    {
        return false;
    }
    true
}

fn do_act(action: &str, id: &str) -> ActReply {
    let Some(path) = cgroup::resolve_id(id) else {
        return ActReply {
            ok: false,
            msg: format!("unknown or invalid id: {}", id),
        };
    };
    let raw = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = cgroup::cgroup_to_app_name(&raw);

    // Every action that freezes, limits, kills, or pins a target's memory is
    // gated on the full-path safety check. (thaw/uncap only *undo* limits, so
    // they stay permissive — worst case they lift a restriction.)
    if matches!(
        action,
        "freeze" | "cap" | "kill" | "protect" | "unprotect"
    ) && !permitted_target(&path, action)
    {
        return ActReply {
            ok: false,
            msg: format!("{} refused: {} is not a permitted target", action, name),
        };
    }

    let res: anyhow::Result<String> = match action {
        "freeze" => actions::freeze_cgroup(&path).map(|_| format!("Paused {}", name)),
        "thaw" => actions::thaw_cgroup(&path).map(|_| format!("Resumed {}", name)),
        "cap" => {
            let cur = cgroup::read_cgroup_u64(&path, "memory.current").unwrap_or(0);
            let cap = (cur / 2).max(128 * 1024 * 1024); // half current, floor 128 MB
            actions::cap_cgroup(&path, cap).map(|_| format!("Capped {}", name))
        }
        "uncap" => actions::uncap_cgroup(&path).map(|_| format!("Uncapped {}", name)),
        "kill" => actions::kill_cgroup(&path).map(|_| format!("Closed {}", name)),
        "protect" => guard::protect_cgroup(&path).map(|_| format!("Protected {}", name)),
        "unprotect" => guard::unprotect_cgroup(&path).map(|_| format!("Unprotected {}", name)),
        other => {
            return ActReply {
                ok: false,
                msg: format!("unknown action: {}", other),
            }
        }
    };
    match res {
        Ok(msg) => {
            crate::events::record(msg.clone());
            ActReply { ok: true, msg }
        }
        Err(e) => ActReply {
            ok: false,
            msg: format!("{} failed: {}", action, e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::permitted_target;
    use std::path::Path;

    const CHROME: &str =
        "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.google.Chrome-1234.scope";
    const USER_MGR: &str =
        "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service";
    const OLLAMA: &str = "/sys/fs/cgroup/system.slice/ollama.service";

    // Slices, the root, and init.scope must be refused for any action.
    #[test]
    fn slices_root_and_init_are_refused() {
        for a in ["freeze", "kill", "protect"] {
            assert!(!permitted_target(Path::new("/sys/fs/cgroup"), a));
            assert!(!permitted_target(Path::new("/sys/fs/cgroup/user.slice"), a));
            assert!(!permitted_target(Path::new("/sys/fs/cgroup/system.slice"), a));
            assert!(!permitted_target(Path::new("/sys/fs/cgroup/init.scope"), a));
        }
    }

    // The systemd --user manager (freezing it = unrecoverable session lockup)
    // must be refused as a TARGET, even though it's a legit app ancestor.
    #[test]
    fn user_manager_is_refused() {
        for a in ["freeze", "kill", "cap"] {
            assert!(!permitted_target(Path::new(USER_MGR), a));
        }
    }

    // Session-critical units stay refused even reached through a full path.
    #[test]
    fn critical_units_are_refused_by_component() {
        assert!(!permitted_target(Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/session.slice/org.gnome.Shell@wayland.service"
        ), "freeze"));
        assert!(!permitted_target(Path::new("/sys/fs/cgroup/system.slice/pipewire.service"), "freeze"));
    }

    // A user's own app is fully actionable.
    #[test]
    fn ordinary_app_unit_is_permitted() {
        for a in ["freeze", "kill", "cap", "protect"] {
            assert!(permitted_target(Path::new(CHROME), a));
        }
    }

    // A system service (e.g. ollama) may be *paused* (reversible) but never
    // *killed* by an unprivileged client through the root daemon.
    #[test]
    fn system_service_freezable_not_killable() {
        assert!(permitted_target(Path::new(OLLAMA), "freeze"));
        assert!(permitted_target(Path::new(OLLAMA), "cap"));
        assert!(!permitted_target(Path::new(OLLAMA), "kill"));
    }
}
