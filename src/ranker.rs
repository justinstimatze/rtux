use anyhow::Result;
use std::path::PathBuf;

use crate::cgroup;

#[derive(Debug)]
pub struct AppUsage {
    pub name: String,
    /// Kept for callers that want to act on the ranked app; the notifier only
    /// needs the name + size.
    #[allow(dead_code)]
    pub cgroup_path: PathBuf,
    pub memory_bytes: u64,
}

/// Get all app cgroups sorted by memory usage (highest first).
pub fn rank_apps() -> Result<Vec<AppUsage>> {
    let cgroups = cgroup::list_app_cgroups()?;
    let mut apps: Vec<AppUsage> = cgroups
        .into_iter()
        .filter_map(|(path, name)| {
            let mem = cgroup::read_cgroup_u64(&path, "memory.current").ok()?;
            // Skip tiny cgroups (< 1MB)
            if mem < 1024 * 1024 {
                return None;
            }
            Some(AppUsage {
                name,
                cgroup_path: path,
                memory_bytes: mem,
            })
        })
        .collect();
    apps.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
    Ok(apps)
}
