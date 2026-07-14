use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const CGROUP_BASE: &str = "/sys/fs/cgroup";

/// Walk the cgroup tree to find a service matching one of the given names.
/// Returns the cgroup directory path (e.g. /sys/fs/cgroup/user.slice/.../org.gnome.Shell@wayland.service).
pub fn find_cgroup_for_service(service_names: &[&str]) -> Result<Option<PathBuf>> {
    let user_slice = PathBuf::from(CGROUP_BASE).join("user.slice");
    if !user_slice.exists() {
        bail!("cgroups v2 user.slice not found at {}", user_slice.display());
    }
    find_service_recursive(&user_slice, service_names)
}

fn find_service_recursive(dir: &Path, names: &[&str]) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        for svc in names {
            if name_str.contains(svc) {
                return Ok(Some(path));
            }
        }
        if let Some(found) = find_service_recursive(&path, names)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
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
/// (e.g. "claude (rtux)") so a kill notification says *which* session died and
/// the user can resume it. None if it isn't a Claude session.
pub fn claude_session_label(cgroup_path: &Path) -> Option<String> {
    let procs = fs::read_to_string(cgroup_path.join("cgroup.procs")).ok()?;
    for line in procs.lines() {
        let Ok(pid) = line.trim().parse::<i32>() else { continue };
        let comm = fs::read_to_string(format!("/proc/{}/comm", pid)).unwrap_or_default();
        if comm.trim() == "claude" {
            let dir = fs::read_link(format!("/proc/{}/cwd", pid))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "?".to_string());
            return Some(format!("claude ({})", dir));
        }
    }
    None
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
                if mem > 0 {
                    out.push((path.clone(), cgroup_to_app_name(&fname), mem));
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

    // VTE terminal child processes: vte-spawn-<UUID> -> "Terminal (child)"
    if s.starts_with("vte-spawn-") || s.starts_with("Vte-spawn-") {
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
