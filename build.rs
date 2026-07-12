use std::process::Command;

/// The git tag is the single source of truth for the version. `git describe`
/// gives `v0.1.0` at a tag and `v0.1.0-3-gabc1234` three commits later; a dirty
/// tree gets `-dirty`. Outside a git tree (a source tarball) we fall back to the
/// Cargo.toml version. No hand-maintained version constant to drift.
fn main() {
    let version = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));

    println!("cargo:rustc-env=RTUX_VERSION={version}");
    // Rebuild the version string when the commit or tags change.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
}
