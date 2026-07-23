use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const CGROUP_BASE: &str = "/sys/fs/cgroup";

/// Every cgroup matching any of `service_names` — not just the first.
///
/// A category can legitimately span several units, and stopping at the first
/// match silently leaves the rest unprotected. Measured on 2026-07-14: "pipewire"
/// matches BOTH `pipewire-pulse.service` and `pipewire.service`; the search found
/// pulse, reported "protected audio", and left the actual audio daemon sitting at
/// oom_score_adj=+200 as OOM meat — while the startup log cheerfully claimed audio
/// was protected. Protect them all; a category is a set, not a representative.
pub fn find_all_cgroups_for_service(service_names: &[&str]) -> Result<Vec<PathBuf>> {
    let user_slice = PathBuf::from(CGROUP_BASE).join("user.slice");
    if !user_slice.exists() {
        bail!("cgroups v2 user.slice not found at {}", user_slice.display());
    }
    let mut out = Vec::new();
    collect_services_recursive(&user_slice, service_names, &mut out);
    Ok(out)
}

fn collect_services_recursive(dir: &Path, names: &[&str], out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name_str = entry.file_name().to_string_lossy().to_string();
        if names.iter().any(|svc| name_str.contains(svc)) {
            out.push(path.clone());
            // Don't descend into a matched unit: its own leaf is the target.
            continue;
        }
        collect_services_recursive(&path, names, out);
    }
}

/// Read a cgroup knob (e.g. memory.current) and return its value as bytes.
pub fn read_cgroup_u64(cgroup_path: &Path, knob: &str) -> Result<u64> {
    let path = cgroup_path.join(knob);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let val = content.trim();
    if val == "max" {
        return Ok(u64::MAX);
    }
    val.parse::<u64>()
        .with_context(|| format!("parsing {} from {}", val, path.display()))
}

/// One field out of `memory.stat`, which is `key value` lines rather than a bare
/// number — so `read_cgroup_u64` cannot reach it.
///
/// Returns None rather than an error: every caller so far samples this on a timer
/// against a cgroup that may vanish mid-flight (a service restarts, a session
/// ends), and a missing counter is a normal event there, not a fault to report.
pub fn read_memory_stat_field(cgroup_path: &Path, field: &str) -> Option<u64> {
    let content = fs::read_to_string(cgroup_path.join("memory.stat")).ok()?;
    content.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(' ')?;
        rest.trim().parse::<u64>().ok()
    })
}

/// `usage_usec` out of `cpu.stat` — total CPU time (microseconds, summed across
/// cores) this cgroup has ever consumed. Monotonic, so utilisation is a *delta*
/// over a window, never the raw total (the same d/dt discipline as the fault
/// meter). Same `key value` line format as `memory.stat`. None if absent — the
/// cpu controller may not be enabled here, or the cgroup may have vanished
/// mid-sample, both normal on a timer.
pub fn read_cpu_usage_usec(cgroup_path: &Path) -> Option<u64> {
    let content = fs::read_to_string(cgroup_path.join("cpu.stat")).ok()?;
    content.lines().find_map(|line| {
        let rest = line.strip_prefix("usage_usec")?.strip_prefix(' ')?;
        rest.trim().parse::<u64>().ok()
    })
}

/// Write a value to a cgroup knob.
pub fn write_cgroup(cgroup_path: &Path, knob: &str, value: &str) -> Result<()> {
    let path = cgroup_path.join(knob);
    fs::write(&path, value)
        .with_context(|| format!("writing '{}' to {}", value, path.display()))
}

/// List app-slice scopes (cgroups under app.slice) for the current user.
/// Returns (cgroup_path, human_name) pairs.
pub fn list_app_cgroups() -> Result<Vec<(PathBuf, String)>> {
    let uid = nix::unistd::getuid();
    let app_slice = PathBuf::from(CGROUP_BASE)
        .join("user.slice")
        .join(format!("user-{}.slice", uid))
        .join(format!("user@{}.service", uid))
        .join("app.slice");

    if !app_slice.exists() {
        return Ok(Vec::new());
    }

    let mut apps = Vec::new();
    collect_leaf_cgroups(&app_slice, &mut apps)?;
    Ok(apps)
}

fn collect_leaf_cgroups(dir: &Path, results: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Check if this dir has memory.current (i.e., it's a cgroup with processes)
        if path.join("memory.current").exists() {
            let name = cgroup_to_app_name(&entry.file_name().to_string_lossy());
            results.push((path.clone(), name));
        }
        collect_leaf_cgroups(&path, results)?;
    }
    Ok(())
}

/// A significant memory-consuming unit cgroup, with everything the HUD needs to
/// render its row and decide which actions are live.
pub struct AppInfo {
    pub path: PathBuf,
    pub raw: String,   // raw cgroup dir name (for denylist matching)
    pub name: String,  // humanized
    pub mem: u64,      // memory.current
    pub swap: u64,     // memory.swap.current
    pub has_freeze: bool,
    pub frozen: bool,
    pub mem_min: u64,  // memory.min (>0 ⇒ protected)
}

/// List all unit cgroups (.scope/.service) using at least `min_bytes`, across
/// every user's app.slice and system.slice, sorted largest-first.
pub fn list_apps(min_bytes: u64) -> Vec<AppInfo> {
    let mut roots: Vec<PathBuf> = vec![PathBuf::from(CGROUP_BASE).join("system.slice")];
    let user_slice = PathBuf::from(CGROUP_BASE).join("user.slice");
    if let Ok(entries) = fs::read_dir(&user_slice) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("user-") || !e.path().is_dir() {
                continue;
            }
            if let Ok(sub) = fs::read_dir(e.path()) {
                for s in sub.flatten() {
                    let sname = s.file_name().to_string_lossy().to_string();
                    if sname.starts_with("user@") && sname.ends_with(".service") {
                        roots.push(s.path().join("app.slice"));
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for root in roots {
        if root.exists() {
            collect_apps(&root, min_bytes, &mut out);
        }
    }
    out.sort_by(|a, b| b.mem.cmp(&a.mem));
    out
}

fn collect_apps(dir: &Path, min_bytes: u64, out: &mut Vec<AppInfo>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let raw = entry.file_name().to_string_lossy().to_string();
        if raw.ends_with(".scope") || raw.ends_with(".service") {
            if let Ok(mem) = read_cgroup_u64(&path, "memory.current") {
                if mem >= min_bytes {
                    let has_freeze = path.join("cgroup.freeze").exists();
                    let frozen = has_freeze
                        && read_cgroup_u64(&path, "cgroup.freeze").unwrap_or(0) == 1;
                    out.push(AppInfo {
                        raw: raw.clone(),
                        name: cgroup_to_app_name(&raw),
                        mem,
                        swap: read_cgroup_u64(&path, "memory.swap.current").unwrap_or(0),
                        has_freeze,
                        frozen,
                        mem_min: read_cgroup_u64(&path, "memory.min").unwrap_or(0),
                        path: path.clone(),
                    });
                }
            }
        }
        collect_apps(&path, min_bytes, out);
    }
}

/// Resolve a HUD app id (cgroup path relative to /sys/fs/cgroup) back to an
/// absolute path, rejecting traversal outside the cgroup tree.
pub fn resolve_id(id: &str) -> Option<PathBuf> {
    if id.contains("..") {
        return None;
    }
    let p = PathBuf::from(CGROUP_BASE).join(id.trim_start_matches('/'));
    if p.starts_with(CGROUP_BASE) && p.join("cgroup.controllers").exists() {
        Some(p)
    } else {
        None
    }
}

/// Resident set size (bytes) of a pid, from /proc/<pid>/statm (field 1 = pages).
fn rss_bytes(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{}/statm", pid))
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|f| f.parse::<u64>().ok()))
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

/// Build a meaningful label for a generic terminal ("vte-spawn") scope by looking
/// at the largest process inside it: its command, and the project directory it's
/// running in. Turns a wall of identical "Terminal (child)" rows into e.g.
/// "claude · rtux" / "node · publicai" so they can be told apart.
pub fn proc_label(cgroup_path: &Path) -> Option<String> {
    // A Claude session is the common case in terminal scopes and has a canonical
    // directory-qualified label — share it so the HUD and the kill witness agree.
    if let Some(label) = claude_session_label(cgroup_path) {
        return Some(label);
    }
    largest_proc_label(cgroup_path)
}

/// `proc_label` minus the Claude question: name the scope after its largest
/// process ("MainThread · web", "node · publicai").
///
/// Split out so the kill path can reach it *without* re-asking whether this is a
/// Claude session. `judgment::assess` has already answered that question to pick
/// the eviction rank, and calling `proc_label` there would ask a second time —
/// re-reading `/proc` mid-kill, where a process set that changed between the two
/// reads yields a victim ranked Ordinary but announced as "claude · dir". Keeping
/// the two halves callable separately is what makes rank and label agree by
/// construction rather than by luck.
pub fn largest_proc_label(cgroup_path: &Path) -> Option<String> {
    let procs = fs::read_to_string(cgroup_path.join("cgroup.procs")).ok()?;
    let mut best_pid = 0i32;
    let mut best_rss = 0u64;
    for line in procs.lines().take(128) {
        let pid: i32 = match line.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rss = rss_bytes(pid);
        if rss > best_rss {
            best_rss = rss;
            best_pid = pid;
        }
    }
    if best_pid == 0 {
        return None;
    }
    let comm = fs::read_to_string(format!("/proc/{}/comm", best_pid))
        .ok()?
        .trim()
        .to_string();
    let cmdline = fs::read(format!("/proc/{}/cmdline", best_pid))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default();
    // Claude Code runs as node/bun — surface it as "claude" when detectable.
    let base = if cmdline.contains("claude") {
        "claude".to_string()
    } else {
        comm
    };
    // The working directory basename is the best disambiguator between sessions.
    let cwd = fs::read_link(format!("/proc/{}/cwd", best_pid))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .filter(|d| !d.is_empty() && d != "/");
    Some(match cwd {
        Some(dir) if dir != base => format!("{} · {}", base, dir),
        _ => base,
    })
}

/// The cgroup directory of an arbitrary pid (from /proc/<pid>/cgroup).
pub fn cgroup_of_pid(pid: i32) -> Option<PathBuf> {
    let content = fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()?;
    for line in content.lines() {
        if let Some(p) = line.strip_prefix("0::") {
            return Some(Path::new(CGROUP_BASE).join(p.trim().trim_start_matches('/')));
        }
    }
    None
}

/// The parent pid of `pid`, from /proc/<pid>/stat. The comm field (2nd) can
/// contain spaces and parens, so we index from the *last* ')': after it come
/// state (field 3) then ppid (field 4).
fn ppid_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Socket inodes held by any process in this cgroup, from `/proc/<pid>/fd`.
/// Bounded: a runaway fd table cannot make this walk unbounded work.
fn socket_inodes_of(cgroup_path: &Path) -> std::collections::HashSet<u64> {
    const MAX_FDS: usize = 4096;
    let mut out = std::collections::HashSet::new();
    let Ok(procs) = fs::read_to_string(cgroup_path.join("cgroup.procs")) else {
        return out;
    };
    let mut seen = 0usize;
    for pid in procs.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
        let Ok(fds) = fs::read_dir(format!("/proc/{}/fd", pid)) else {
            continue; // process ended, or not ours to read
        };
        for fd in fds.flatten() {
            if seen >= MAX_FDS {
                return out;
            }
            seen += 1;
            if let Ok(target) = fs::read_link(fd.path()) {
                if let Some(ino) = target
                    .to_string_lossy()
                    .strip_prefix("socket:[")
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    out.insert(ino);
                }
            }
        }
    }
    out
}

/// Is this cgroup a local server with a client connected to it *right now*?
///
/// Freezing a server is not like freezing a client. `cgroup.freeze` is SIGSTOP with
/// no timeout, so a peer waiting on a reply waits forever — there is no error to
/// handle and no deadline to trip, only an indefinite stall that ends when something
/// else happens to thaw it. A local model server makes this concrete: freeze it
/// mid-embedding and whatever asked for that embedding hangs, and on this box the
/// callers are the very sessions the freeze rung is also pausing.
///
/// The predicate is deliberately narrow: **listening, with an established loopback
/// peer**. "Has network connections" would be far too broad — a browser and every
/// agent session hold outbound sockets constantly, so that test would empty the
/// candidate list and leave the rung nothing to freeze under real pressure.
/// Direction is what distinguishes a server someone is waiting on from a client
/// doing its own IO.
///
/// An idle server is still freezable, and should be: it is the cheapest memory on
/// the box. Measured 2026-07-22 — a local model server frozen and reclaimed went
/// from 802MB to 7.3MB resident, and pages return only on demand. This gate does not
/// protect servers, it protects *in-flight requests*.
///
/// Restricted to `.service` units to bound the cost: it is the fd walk that is
/// expensive, and confining it to services keeps browsers and agent scopes — the
/// ones with thousands of fds — from ever paying for it.
pub fn serving_local_clients(cgroup_path: &Path) -> bool {
    let is_service = cgroup_path
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(".service"))
        .unwrap_or(false);
    if !is_service {
        return false;
    }
    let inodes = socket_inodes_of(cgroup_path);
    if inodes.is_empty() {
        return false;
    }
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = fs::read_to_string(table) else {
            continue;
        };
        // Pass 1: which ports does THIS cgroup listen on?
        let mut ports = std::collections::HashSet::new();
        for f in content.lines().skip(1).map(tcp_fields) {
            if let Some((local, _rem, st, ino)) = f {
                if st == TCP_LISTEN && inodes.contains(&ino) {
                    ports.insert(local.1);
                }
            }
        }
        if ports.is_empty() {
            continue;
        }
        // Pass 2: is anything on this machine connected to one of them? A server-side
        // established socket has the listening port as its LOCAL port.
        for f in content.lines().skip(1).map(tcp_fields) {
            if let Some((local, rem, st, _ino)) = f {
                if st == TCP_ESTABLISHED && ports.contains(&local.1) && is_loopback_hex(rem.0) {
                    return true;
                }
            }
        }
    }
    false
}

const TCP_ESTABLISHED: u8 = 0x01;
const TCP_LISTEN: u8 = 0x0A;

/// `(local_addr_hex, local_port), (rem_addr_hex, rem_port), state, inode` out of one
/// `/proc/net/tcp{,6}` row. None on any malformed line — this is a kernel table, but
/// a parse failure must never panic the daemon.
fn tcp_fields(line: &str) -> Option<((&str, u16), (&str, u16), u8, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 10 {
        return None;
    }
    let (la, lp) = f[1].split_once(':')?;
    let (ra, rp) = f[2].split_once(':')?;
    Some((
        (la, u16::from_str_radix(lp, 16).ok()?),
        (ra, u16::from_str_radix(rp, 16).ok()?),
        u8::from_str_radix(f[3], 16).ok()?,
        f[9].parse().ok()?,
    ))
}

/// Is this `/proc/net/tcp` hex address the loopback interface? `127.0.0.1` is stored
/// little-endian as `0100007F`; IPv6 `::1` is the 32-hex-digit form. Anything else is
/// a remote peer, which does not count — a client on another machine waiting on a
/// reply is not the local stall this gate exists to prevent.
fn is_loopback_hex(addr: &str) -> bool {
    addr.eq_ignore_ascii_case("0100007F")
        || addr.eq_ignore_ascii_case("00000000000000000000000001000000")
}

/// Is this scope a tmux pane — a `tmux-spawn-*` cgroup systemd created for a pane's
/// process? These are the scopes ancestry cannot reach from a focused window (see
/// `focus_owns_a_tmux_client`).
pub fn is_tmux_pane_scope(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with("tmux-spawn"))
        .unwrap_or(false)
}

/// Does the focused window own a tmux *client*?
///
/// The join that does not exist. A tmux pane's process descends from the tmux
/// SERVER, which systemd owns directly — measured 2026-07-22:
///
///     tmux-spawn-<uuid>.scope  ->  tmux: server  ->  systemd (user manager)
///                                                    ^ not the terminal
///
/// so no ancestry walk from a focused terminal ever reaches a pane. The client is
/// the other half, and it IS reachable:
///
///     tmux: client  ->  bash  ->  terminal   <- the focused window
///
/// but nothing joins a client to its panes outside tmux itself: panes carry
/// `TMUX=<socket>,<server-pid>,<session-id>` in their environ while clients carry no
/// session marker in either environ or cmdline. Querying the server for the map means
/// a root daemon shelling out to tmux as the user, which is not worth it here.
///
/// So this answers the weaker question — "is the window you just focused a terminal
/// with tmux running in it?" — and the caller treats a yes as covering every pane.
/// Deliberately coarse, and deliberately scoped to the THAW path only: widening the
/// freeze path's `is_foreground_related` this way would make every pane unfreezable
/// whenever a terminal has focus — continuously, for a terminal-centric user — leaving
/// the rung no
/// victims at all. Thawing too much is recoverable in one settle window; having
/// nothing to freeze under real pressure is not.
pub fn focus_owns_a_tmux_client(fg_pid: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue; // not a process directory
        };
        // "tmux: client" vs "tmux: server" — the server must NOT match, or every
        // pane would look reachable from any focus at all.
        match fs::read_to_string(format!("/proc/{}/comm", pid)) {
            Ok(comm) if comm.trim() == "tmux: client" => {}
            _ => continue,
        }
        if pid_descends_from(pid, fg_pid) {
            return true;
        }
    }
    false
}

/// True if `pid` is `ancestor` or descends from it via the parent chain. Used to
/// spare the *foreground* terminal and all its tabs: the focus tracker reports
/// the terminal window's pid, and its shell/agent children live in sibling
/// vte-spawn scopes whose processes descend from it. Bounded walk (cycles/depth).
pub fn pid_descends_from(mut pid: i32, ancestor: i32) -> bool {
    if ancestor <= 0 {
        return false;
    }
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        match ppid_of(pid) {
            Some(p) => pid = p,
            None => return false,
        }
    }
    false
}

/// If this cgroup hosts a Claude Code session, return a directory-qualified label
/// (e.g. "claude · rtux") so a kill notification says *which* session died and
/// the user can resume it. None if it isn't a Claude session.
///
/// Robust to teardown races: the directory is read from *any* process in the
/// scope, not just the claude process. They share the project cwd (the shell,
/// tmux, and child procs all sit in it), so a single unreadable `/proc/pid/cwd`
/// — common at kill time under heavy pressure — can't erase the name. This was
/// a real miss: two sessions were killed and logged as a bare "claude" because
/// the claude proc's cwd raced with its teardown.
pub fn claude_session_label(cgroup_path: &Path) -> Option<String> {
    let procs = fs::read_to_string(cgroup_path.join("cgroup.procs")).ok()?;
    let pids: Vec<i32> = procs.lines().filter_map(|l| l.trim().parse().ok()).collect();

    let is_claude = pids.iter().any(|&p| {
        fs::read_to_string(format!("/proc/{}/comm", p))
            .map(|c| c.trim() == "claude")
            .unwrap_or(false)
    });
    if !is_claude {
        return None;
    }

    // First readable, meaningful cwd from any proc in the scope.
    let dir = pids.iter().find_map(|&p| {
        fs::read_link(format!("/proc/{}/cwd", p))
            .ok()
            .and_then(|pp| pp.file_name().map(|n| n.to_string_lossy().to_string()))
            .filter(|d| !d.is_empty() && d != "/")
    });
    Some(match dir {
        Some(d) => format!("claude · {}", d),
        None => "claude".to_string(),
    })
}

/// Fraction of swap in use (0.0–1.0), from /proc/meminfo. When this nears 1.0
/// there's nowhere left to reclaim to — freeze-to-zram is futile and the machine
/// thrashes — so it's a hard trigger to escalate straight to the kill rung.
pub fn swap_used_fraction() -> f64 {
    let Ok(mi) = fs::read_to_string("/proc/meminfo") else { return 0.0 };
    let kb = |rest: &str| -> u64 { rest.trim().trim_end_matches("kB").trim().parse().unwrap_or(0) };
    let (mut total, mut free) = (0u64, 0u64);
    for line in mi.lines() {
        if let Some(r) = line.strip_prefix("SwapTotal:") {
            total = kb(r);
        } else if let Some(r) = line.strip_prefix("SwapFree:") {
            free = kb(r);
        }
    }
    if total == 0 {
        0.0
    } else {
        total.saturating_sub(free) as f64 / total as f64
    }
}

/// Enable the `cpu` controller on every ancestor of `leaf` from the cgroup root
/// down to leaf's parent, so `cpu.weight` becomes writable on `leaf` (and on
/// leaf's parent). cgroup v2 exposes a controller's knobs on a cgroup only if
/// the controller is in its parent's `cgroup.subtree_control`, and it must be
/// enabled top-down. Best-effort: a level that already has it is skipped, and a
/// level with processes directly in it will refuse (the no-internal-process
/// rule) — neither is fatal.
pub fn ensure_cpu_controller(leaf: &Path) {
    let base = Path::new(CGROUP_BASE);
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cur = leaf.parent();
    while let Some(p) = cur {
        if !p.starts_with(base) {
            break;
        }
        chain.push(p.to_path_buf());
        if p == base {
            break;
        }
        cur = p.parent();
    }
    chain.reverse(); // top-down: root first, then each descendant slice
    for cg in chain {
        let sc = cg.join("cgroup.subtree_control");
        if let Ok(cur) = fs::read_to_string(&sc) {
            if !cur.split_whitespace().any(|c| c == "cpu") {
                let _ = fs::write(&sc, "+cpu");
            }
        }
    }
}

/// The cgroup directory of the current process — used to avoid freezing ourselves.
pub fn self_cgroup() -> Option<PathBuf> {
    let content = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(Path::new(CGROUP_BASE).join(path.trim().trim_start_matches('/')));
        }
    }
    None
}

/// List freezable *unit* cgroups (.scope / .service) under every user's app.slice
/// and under system.slice, paired with their recursive memory.current, sorted
/// largest-first. These are the candidate targets for pressure mitigation.
/// Slices are intentionally excluded (freezing a slice would pause everything in it).
pub fn list_freezable_cgroups() -> Result<Vec<(PathBuf, String, u64)>> {
    let mut roots: Vec<PathBuf> = vec![PathBuf::from(CGROUP_BASE).join("system.slice")];

    // Every logged-in user's app.slice (works whether we run as root or the user).
    let user_slice = PathBuf::from(CGROUP_BASE).join("user.slice");
    if let Ok(entries) = fs::read_dir(&user_slice) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("user-") || !e.path().is_dir() {
                continue;
            }
            if let Ok(sub) = fs::read_dir(e.path()) {
                for s in sub.flatten() {
                    let sname = s.file_name().to_string_lossy().to_string();
                    if sname.starts_with("user@") && sname.ends_with(".service") {
                        roots.push(s.path().join("app.slice"));
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for root in roots {
        if root.exists() {
            collect_freezable(&root, &mut out);
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2));
    Ok(out)
}

fn collect_freezable(dir: &Path, out: &mut Vec<(PathBuf, String, u64)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().to_string();
        // Only real units are freezable targets — never slices.
        let is_unit = fname.ends_with(".scope") || fname.ends_with(".service");
        if is_unit && path.join("cgroup.freeze").exists() {
            if let Ok(mem) = read_cgroup_u64(&path, "memory.current") {
                // Rank by TOTAL FOOTPRINT (resident + swapped), not memory.current.
                //
                // memory.current counts only what is resident *right now*, so it
                // COLLAPSES as a cgroup gets swapped out — under the exact pressure
                // this daemon exists to handle. On 2026-07-14 that blindness cost a
                // whole session: rtux sat at critical for three minutes having frozen
                // twice, because every real hog had paged out below MIN_FREEZE_BYTES
                // (one showed 857MB resident against 1.5GB swapped — a 2.4GB process
                // ranked as 857MB, and a 479MB/45MB one fell under the 512MB floor
                // entirely). The candidate list sorts largest-first and the rungs
                // `break` at the floor, so everything looked small and both the freeze
                // and kill rungs found nothing to do — right up until the kernel's
                // global OOM killer took systemd --user and logged the user out.
                //
                // A swapped-out page is not a freed page: the cgroup still owns it and
                // will fault it back. Footprint is what the machine must actually hold,
                // so it is the honest thing to rank and gate on.
                let swap = read_cgroup_u64(&path, "memory.swap.current").unwrap_or(0);
                let footprint = mem.saturating_add(swap);
                if footprint > 0 {
                    out.push((path.clone(), cgroup_to_app_name(&fname), footprint));
                }
            }
        }
        collect_freezable(&path, out);
    }
}

/// Convert a cgroup scope name to a human-readable app name.
/// e.g. "snap.firefox.firefox-1234.scope" -> "Firefox"
/// e.g. "app-gnome-org.gnome.Terminal-12345.scope" -> "Terminal"
pub fn cgroup_to_app_name(scope: &str) -> String {
    let raw = scope.trim_end_matches(".scope").trim_end_matches(".service").trim_end_matches(".slice");
    let s = unescape_systemd(raw);

    // Terminal child scopes: vte-spawn-<UUID> / tmux-spawn-<UUID> -> generic
    // fallback. (Callers that want the richer "claude · dir" label run proc_label
    // for these; this is only the bare-name fallback.)
    if s.starts_with("vte-spawn-")
        || s.starts_with("Vte-spawn-")
        || s.starts_with("tmux-spawn-")
    {
        return "Terminal (child)".to_string();
    }

    // snap.appname.appname-1234
    if let Some(rest) = s.strip_prefix("snap.") {
        if let Some(app) = rest.split('.').next() {
            return capitalize(app);
        }
    }

    // app-gnome-org.gnome.AppName-12345 or app-gnome-update-notifier-12345
    if let Some(rest) = s.strip_prefix("app-gnome-") {
        let name = strip_trailing_pid(rest);
        // If it contains dots (org.gnome.Foo), take the last segment
        if name.contains('.') {
            if let Some(last) = name.rsplit('.').next() {
                return capitalize(last);
            }
        }
        return capitalize(name);
    }

    // app-flatpak-com.example.AppName-12345
    if let Some(rest) = s.strip_prefix("app-flatpak-") {
        let name = strip_trailing_pid(rest);
        if let Some(last) = name.rsplit('.').next() {
            return capitalize(last);
        }
        return capitalize(name);
    }

    // Fallback: strip common prefixes/suffixes
    let cleaned = s
        .trim_start_matches("app-")
        .trim_start_matches("gnome-");
    let name = strip_trailing_pid(cleaned);
    // Reverse-DNS app-ids (com.google.Chrome, org.foo.Bar) read as their last
    // segment — otherwise the whole id shows, capitalized like a sentence.
    if name.contains('.') {
        if let Some(last) = name.rsplit('.').next().filter(|l| !l.is_empty()) {
            return capitalize(last);
        }
    }
    capitalize(name)
}

/// Strip a trailing -PID (numeric suffix) from a name.
fn strip_trailing_pid(s: &str) -> &str {
    if let Some(pos) = s.rfind('-') {
        if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].is_empty() {
            return &s[..pos];
        }
    }
    s
}

fn unescape_systemd(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.as_str().starts_with("x2d") {
            result.push('-');
            chars.next(); chars.next(); chars.next();
        } else {
            result.push(c);
        }
    }
    result
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Get total system RAM in bytes from /proc/meminfo.
pub fn total_ram_bytes() -> Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo")
        .context("reading /proc/meminfo")?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest.trim().trim_end_matches("kB").trim();
            let kb: u64 = kb_str.parse().context("parsing MemTotal")?;
            return Ok(kb * 1024);
        }
    }
    bail!("MemTotal not found in /proc/meminfo")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scopes ancestry cannot reach. `tmux-spawn-*` gets the coarse fallback;
    /// `vte-spawn-*` and app scopes must NOT, since ancestry already answers for them
    /// exactly and widening it would thaw panes on any focus at all.
    #[test]
    fn only_tmux_pane_scopes_take_the_coarse_path() {
        let p = |s: &str| PathBuf::from("/sys/fs/cgroup/user.slice/app.slice").join(s);
        assert!(is_tmux_pane_scope(&p("tmux-spawn-64c81969-5a39.scope")));
        assert!(!is_tmux_pane_scope(&p("vte-spawn-674b05ed-e8ca.scope")));
        assert!(!is_tmux_pane_scope(&p("app-gnome-terminator-1234.scope")));
        assert!(!is_tmux_pane_scope(&p("ollama.service")));
    }
}

#[cfg(test)]
mod net_tests {
    use super::*;

    /// Real rows off this box's /proc/net/tcp. The field offsets are the contract —
    /// state is field 3 and inode is field 9 — and getting either wrong silently
    /// turns the gate into "never fires" or "always fires".
    #[test]
    fn parses_a_listen_and_an_established_row() {
        let listen = "   6: 0100007F:2CAA 00000000:0000 0A 00000000:00000000 00:00000000 00000000   998        0 27063251 2 0000000000000000 100 0 0 10 0";
        let ((la, lp), _, st, ino) = tcp_fields(listen).expect("listen row parses");
        assert_eq!((la, lp), ("0100007F", 11434)); // 0x2CAA
        assert_eq!(st, TCP_LISTEN);
        assert_eq!(ino, 27063251);

        let est = "   0: 0100007F:2CAA 0100007F:8AF0 01 00000000:00000000 00:00000000 00000000  1000        0 24813604 1 0000000000000000 100 0 0 10 0";
        let (_, (ra, _), st, _) = tcp_fields(est).expect("established row parses");
        assert_eq!(st, TCP_ESTABLISHED);
        assert!(is_loopback_hex(ra));
    }

    /// A malformed or truncated row must decline, never panic — this parses a kernel
    /// table on the daemon's hot path.
    #[test]
    fn a_malformed_row_declines_rather_than_panicking() {
        assert!(tcp_fields("").is_none());
        assert!(tcp_fields("garbage").is_none());
        assert!(tcp_fields("  sl  local_address rem_address   st tx_queue").is_none());
    }

    /// Only loopback peers count. A remote client waiting on a reply is not the local
    /// stall this gate exists to prevent, and counting it would spare any listening
    /// service the moment anything on the network touched it.
    #[test]
    fn only_loopback_peers_count() {
        assert!(is_loopback_hex("0100007F"));
        assert!(is_loopback_hex("00000000000000000000000001000000"));
        assert!(!is_loopback_hex("00000000")); // 0.0.0.0 — a bind address, not a peer
        assert!(!is_loopback_hex("0101A8C0")); // 192.168.1.1
    }
}
