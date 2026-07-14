use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::cgroup;

const CGROUP_BASE: &str = "/sys/fs/cgroup";

/// Bias systemd-oomd against killing a protected cgroup. This is the crucial
/// companion to `memory.min`: memory.min only fends off *kernel* page reclaim,
/// but systemd-oomd's pressure-triggered SIGKILL ignores it entirely — which is
/// how oomd tore down the compositor cgroup out from under us mid-mitigation.
/// The `user.oomd_avoid` xattr is oomd's ManagedOOMPreference=avoid: oomd will
/// pick such a cgroup only as a last resort, after everything else. Best-effort
/// — silently a no-op on kernels/filesystems that don't take the xattr.
fn set_oomd_avoid(cgroup_path: &Path, avoid: bool) {
    let Ok(cpath) = CString::new(cgroup_path.as_os_str().as_bytes()) else {
        return;
    };
    let name = c"user.oomd_avoid";
    unsafe {
        if avoid {
            let val = b"1";
            let _ = nix::libc::setxattr(
                cpath.as_ptr(),
                name.as_ptr(),
                val.as_ptr() as *const nix::libc::c_void,
                val.len(),
                0,
            );
        } else {
            let _ = nix::libc::removexattr(cpath.as_ptr(), name.as_ptr());
        }
    }
}

// Single source of truth for every memory.min protection we set — compositor,
// audio, HUD pin, and foreground alike — keyed by the protected leaf cgroup.
// Ancestor values are *derived* from this, so a protection can be released in
// full, including the residue it would otherwise leave on shared parent slices.
static PROTECTIONS: LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Every cgroup we've ever written memory.min on, so a recompute can zero the
// ones it no longer needs (rather than leaking them until reboot).
static TOUCHED: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Pure core: given the active protections, the memory.min each cgroup should
/// carry — every protected leaf and its ancestors (up to, but excluding, the
/// cgroup root) take the *max* protection passing through them, because the
/// kernel needs a parent's memory.min >= its protected child's. Testable without
/// touching cgroupfs.
fn desired_min_map(protections: &HashMap<PathBuf, u64>, base: &Path) -> HashMap<PathBuf, u64> {
    let mut desired: HashMap<PathBuf, u64> = HashMap::new();
    for (leaf, &val) in protections {
        let mut cur: Option<&Path> = Some(leaf.as_path());
        while let Some(cg) = cur {
            if cg == base || !cg.starts_with(base) {
                break;
            }
            let e = desired.entry(cg.to_path_buf()).or_insert(0);
            if val > *e {
                *e = val;
            }
            cur = cg.parent();
        }
    }
    desired
}

/// Recompute memory.min across the whole tree from the registry and write it,
/// zeroing any cgroup a prior recompute touched that's no longer needed. This is
/// what makes protections fully reversible.
fn apply() {
    let base = Path::new(CGROUP_BASE);
    let desired = {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        // Self-heal: a protected cgroup that's gone (e.g. a prior HUD process that
        // exited) is dropped, so the registry can't grow without bound and its
        // ancestor residue is released.
        prot.retain(|leaf, _| leaf.exists());
        desired_min_map(&prot, base)
    };
    let mut touched = TOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    for (cg, val) in &desired {
        let _ = cgroup::write_cgroup(cg, "memory.min", &val.to_string());
    }
    for cg in touched.iter() {
        if !desired.contains_key(cg) {
            let _ = cgroup::write_cgroup(cg, "memory.min", "0");
        }
    }
    *touched = desired.keys().cloned().collect();
}

/// Register (or update) a protection, then reconcile the tree. Returns an error
/// only if the *target* itself couldn't be protected (e.g. no write permission),
/// so callers like startup protection can still detect real failure.
fn set_protection(cgroup_path: &Path, value: u64) -> Result<()> {
    {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        prot.insert(cgroup_path.to_path_buf(), value);
    }
    apply();
    // Also tell systemd-oomd to leave this cgroup alone (memory.min doesn't stop
    // oomd — only this xattr does). Set on the protected leaf, not its ancestors,
    // so oomd still has other cgroups to pick from if it must act.
    if value > 0 {
        set_oomd_avoid(cgroup_path, true);
    }
    let got = cgroup::read_cgroup_u64(cgroup_path, "memory.min").unwrap_or(0);
    if value == 0 || got >= value {
        Ok(())
    } else {
        anyhow::bail!("failed to set memory.min on {}", cgroup_path.display())
    }
}

/// Drop a protection, then reconcile — lowering the ancestor residue it leaves
/// behind to whatever the *remaining* protections still need (or zero).
fn clear_protection(cgroup_path: &Path) {
    {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        prot.remove(cgroup_path);
    }
    set_oomd_avoid(cgroup_path, false);
    apply();
}

const COMPOSITOR_SERVICES: &[&str] = &["gnome-shell", "gnome.Shell", "kwin", "sway"];
const AUDIO_SERVICES: &[&str] = &["pipewire", "pulseaudio"];

pub struct ProtectedService {
    pub name: String,
    pub cgroup_path: PathBuf,
    pub memory_min: u64,
}

/// Outcome of one protection pass. Each critical service is attempted
/// independently: `protected` holds those secured this pass, `failed` holds the
/// ones we tried but couldn't (with why), so the caller can report them instead
/// of the failure vanishing.
pub struct ProtectionReport {
    pub protected: Vec<ProtectedService>,
    pub failed: Vec<(&'static str, String)>,
}

/// Calculate memory.min for the compositor based on hardware heuristics.
/// Uses a percentage of total RAM with floor/ceiling:
///   - 3% of total RAM
///   - minimum 256MB
///   - maximum 1GB
fn compositor_memory_min(total_ram: u64) -> u64 {
    let three_pct = total_ram / 33; // ~3%
    let min = 256 * 1024 * 1024; // 256MB
    let max = 1024 * 1024 * 1024; // 1GB
    three_pct.clamp(min, max)
}

/// Calculate memory.min for audio services.
/// Smaller budget: 1% of RAM, floor 64MB, ceiling 256MB.
fn audio_memory_min(total_ram: u64) -> u64 {
    let one_pct = total_ram / 100;
    let min = 64 * 1024 * 1024; // 64MB
    let max = 256 * 1024 * 1024; // 256MB
    one_pct.clamp(min, max)
}

/// Discover and protect compositor + audio services.
/// Returns the list of services that were protected.
pub fn protect_critical_services() -> Result<ProtectionReport> {
    let total_ram = cgroup::total_ram_bytes()?;
    let mut report = ProtectionReport { protected: Vec::new(), failed: Vec::new() };

    // Attempt each service INDEPENDENTLY. A failure in one (its cgroup not up
    // yet, its lookup erroring, or an unwritable memory.min) must never abort
    // protection of the others. This routine previously used `?` on each branch,
    // so an error protecting *audio* discarded an already-successful *compositor*
    // protection — the compositor's memory.min was written as a side effect, but
    // the function returned Err, the daemon reported perpetual failure, and it
    // retried (silently) every 30s forever while the compositor was in fact
    // protected. The compositor is the load-bearing one for responsiveness;
    // never let audio's fate mask it.
    protect_one("compositor", COMPOSITOR_SERVICES, compositor_memory_min(total_ram), &mut report);
    protect_one("audio", AUDIO_SERVICES, audio_memory_min(total_ram), &mut report);

    Ok(report)
}

/// Best-effort protection of one service class. On success it lands in
/// `report.protected`; on any failure it lands in `report.failed` with a reason
/// — nothing is logged here (this runs on a 30s retry, so the *caller* decides
/// when to log, to avoid spamming the journal every cycle).
fn protect_one(name: &'static str, services: &[&str], mem_min: u64, report: &mut ProtectionReport) {
    match cgroup::find_cgroup_for_service(services) {
        Ok(Some(path)) => match set_protection(&path, mem_min) {
            Ok(()) => report.protected.push(ProtectedService {
                name: name.to_string(),
                cgroup_path: path,
                memory_min: mem_min,
            }),
            Err(e) => report.failed.push((name, e.to_string())),
        },
        Ok(None) => report.failed.push((name, "cgroup not present yet".to_string())),
        Err(e) => report.failed.push((name, format!("lookup failed: {e}"))),
    }
}

/// Pin a cgroup's current usage into RAM (protect it from reclaim/swap) by
/// setting memory.min to its live memory.current, propagating up ancestors.
/// Used by the HUD "Protect" action. Clamped to a fraction of RAM — same as
/// protect_foreground — so a client can't pin an arbitrarily large cgroup as
/// unreclaimable and starve the machine into OOM (the opposite of the goal).
pub fn protect_cgroup(cgroup_path: &std::path::Path) -> Result<()> {
    let current = cgroup::read_cgroup_u64(cgroup_path, "memory.current").unwrap_or(0);
    let total = cgroup::total_ram_bytes().unwrap_or(0);
    let cap = if total > 0 { total / 4 } else { 2 * 1024 * 1024 * 1024 };
    set_protection(cgroup_path, current.min(cap))
}

/// Release a protection *fully* — the leaf and any ancestor residue no longer
/// needed by other protections come back down. (Previously only the leaf was
/// cleared, so parent slices accumulated memory.min until reboot.)
pub fn unprotect_cgroup(cgroup_path: &std::path::Path) -> Result<()> {
    clear_protection(cgroup_path);
    Ok(())
}

/// Protect the *focused* app: keep its pages resident so the window you're in is
/// never paged out and always instant. Clamped to a fraction of RAM so a huge
/// foreground app (a big browser) can't reserve the whole machine and starve
/// everything else — foreground is favoured, not made a black hole.
pub fn protect_foreground(cgroup_path: &std::path::Path) -> Result<()> {
    let current = cgroup::read_cgroup_u64(cgroup_path, "memory.current").unwrap_or(0);
    let total = cgroup::total_ram_bytes().unwrap_or(0);
    let cap = if total > 0 { total / 4 } else { 2 * 1024 * 1024 * 1024 };
    set_protection(cgroup_path, current.min(cap))
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::desired_min_map;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    const BASE: &str = "/sys/fs/cgroup";

    #[test]
    fn ancestors_take_max_and_release_cleanly() {
        let base = Path::new(BASE);
        let a = PathBuf::from("/sys/fs/cgroup/user.slice/app.slice/a.scope");
        let b = PathBuf::from("/sys/fs/cgroup/user.slice/app.slice/b.scope");
        let app = Path::new("/sys/fs/cgroup/user.slice/app.slice");
        let user = Path::new("/sys/fs/cgroup/user.slice");

        let mut p = HashMap::new();
        p.insert(a.clone(), 100u64);
        p.insert(b.clone(), 300u64);
        let d = desired_min_map(&p, base);
        assert_eq!(d[&a], 100);
        assert_eq!(d[&b], 300);
        assert_eq!(d[app], 300, "shared ancestor takes the max child");
        assert_eq!(d[user], 300);
        assert!(!d.contains_key(base), "cgroup root is never protected");

        // Releasing the bigger protection lowers the shared ancestors.
        p.remove(&b);
        let d = desired_min_map(&p, base);
        assert_eq!(d[app], 100, "ancestor drops to the remaining protection");
        assert_eq!(d[user], 100);

        // Releasing everything clears the map entirely — no residue.
        p.clear();
        assert!(desired_min_map(&p, base).is_empty());
    }
}
