use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Freeze an entire cgroup subtree via cgroup.freeze (writes "1").
/// Used by the daemon to pause a runaway consumer under critical pressure.
pub fn freeze_cgroup(cgroup_path: &Path) -> Result<()> {
    let freeze_path = cgroup_path.join("cgroup.freeze");
    fs::write(&freeze_path, "1")
        .with_context(|| format!("writing cgroup.freeze=1 to {}", freeze_path.display()))
}

/// Thaw a cgroup subtree previously frozen via cgroup.freeze (writes "0").
pub fn thaw_cgroup(cgroup_path: &Path) -> Result<()> {
    let freeze_path = cgroup_path.join("cgroup.freeze");
    fs::write(&freeze_path, "0")
        .with_context(|| format!("writing cgroup.freeze=0 to {}", freeze_path.display()))
}

/// Throttle a cgroup: set memory.high so the kernel forces it to reclaim its own
/// pages and slows its allocation, without killing it. `bytes` is the soft cap.
pub fn cap_cgroup(cgroup_path: &Path, bytes: u64) -> Result<()> {
    fs::write(cgroup_path.join("memory.high"), bytes.to_string())
        .with_context(|| format!("setting memory.high on {}", cgroup_path.display()))
}

/// Remove a throttle (memory.high = max).
pub fn uncap_cgroup(cgroup_path: &Path) -> Result<()> {
    fs::write(cgroup_path.join("memory.high"), "max")
        .with_context(|| format!("clearing memory.high on {}", cgroup_path.display()))
}

/// Kill an entire cgroup subtree atomically via cgroup.kill (cgroup v2, k5.14+).
pub fn kill_cgroup(cgroup_path: &Path) -> Result<()> {
    fs::write(cgroup_path.join("cgroup.kill"), "1")
        .with_context(|| format!("writing cgroup.kill to {}", cgroup_path.display()))
}

/// Set a cgroup's `cpu.weight` (1..=10000, default 100) — its proportional share
/// of CPU among siblings under contention. Work-conserving: no effect when the
/// cgroup isn't competing for CPU, so a boost costs nothing at idle. Requires the
/// cpu controller enabled on the parent (see cgroup::ensure_cpu_controller).
pub fn set_cpu_weight(cgroup_path: &Path, weight: u32) -> Result<()> {
    let w = weight.clamp(1, 10000);
    fs::write(cgroup_path.join("cpu.weight"), w.to_string())
        .with_context(|| format!("writing cpu.weight to {}", cgroup_path.display()))
}

/// Ask the kernel to reclaim up to `bytes` of cold memory from a cgroup
/// (memory.reclaim, cgroup v2 k5.19+). Anonymous pages go to swap — with zram
/// that's fast compressed RAM. On a *frozen* app this hibernates its working set
/// to zram, freeing physical RAM, reversibly (it faults back on thaw). Achieves
/// CRIU's reclaim goal without CRIU's fragility, and works on any app.
///
/// Best-effort: the kernel frees what it can and may report -EAGAIN if it can't
/// reach the full target, so a short write is not a real failure — hence the
/// caller ignores the result.
pub fn reclaim_cgroup(cgroup_path: &Path, bytes: u64) -> Result<()> {
    fs::write(cgroup_path.join("memory.reclaim"), bytes.to_string())
        .with_context(|| format!("writing memory.reclaim to {}", cgroup_path.display()))
}
