use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use crate::guard::format_bytes;
use crate::ipc::SOCKET_PATH;
use crate::psi;
use crate::ranker;

pub fn cmd_status() -> Result<()> {
    // PSI levels
    let mem_psi = psi::read_psi("/proc/pressure/memory")?;
    let io_psi = psi::read_psi("/proc/pressure/io")?;
    let level = psi::classify_pressure(&mem_psi);

    println!("Pressure level: {}", level);
    println!();
    println!("Memory PSI:");
    println!("  some  avg10={:.2}%  avg60={:.2}%  avg300={:.2}%",
        mem_psi.some.avg10, mem_psi.some.avg60, mem_psi.some.avg300);
    if let Some(full) = mem_psi.full {
        println!("  full  avg10={:.2}%  avg60={:.2}%  avg300={:.2}%",
            full.avg10, full.avg60, full.avg300);
    }
    println!();
    println!("IO PSI:");
    println!("  some  avg10={:.2}%  avg60={:.2}%  avg300={:.2}%",
        io_psi.some.avg10, io_psi.some.avg60, io_psi.some.avg300);
    if let Some(full) = io_psi.full {
        println!("  full  avg10={:.2}%  avg60={:.2}%  avg300={:.2}%",
            full.avg10, full.avg60, full.avg300);
    }
    println!();

    // Top consumers
    println!("Top memory consumers:");
    let apps = ranker::rank_apps()?;
    for (i, app) in apps.iter().take(10).enumerate() {
        println!("  {}. {} — {}", i + 1, app.name, format_bytes(app.memory_bytes));
    }

    Ok(())
}

/// Talk to the running daemon's control socket. `ctl list` renders a terminal
/// HUD of significant apps + their live state; other actions command the daemon.
pub fn cmd_ctl(action: &str, id: Option<&str>) -> Result<()> {
    let req = if action == "list" {
        serde_json::json!({ "cmd": "list" })
    } else {
        let id = id.context("this action needs an app id (get one from `pressured ctl list`)")?;
        serde_json::json!({ "cmd": "act", "action": action, "id": id })
    };

    let mut stream = UnixStream::connect(SOCKET_PATH)
        .with_context(|| format!("connecting to {} (is the daemon running?)", SOCKET_PATH))?;
    writeln!(stream, "{}", req)?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let v: serde_json::Value =
        serde_json::from_str(resp.trim()).context("parsing daemon reply")?;

    if action == "list" {
        render_hud(&v);
    } else {
        let ok = v["ok"].as_bool().unwrap_or(false);
        println!("{} {}", if ok { "✓" } else { "✗" }, v["msg"].as_str().unwrap_or(""));
    }
    Ok(())
}

fn render_hud(v: &serde_json::Value) {
    let some = v["some_avg10"].as_f64().unwrap_or(0.0);
    let full = v["full_avg10"].as_f64().unwrap_or(0.0);
    let gauge = if some > 25.0 || full > 10.0 {
        "CRITICAL"
    } else if some > 5.0 {
        "elevated"
    } else {
        "normal"
    };
    println!("Memory pressure: {} (some avg10={:.1}%, full avg10={:.1}%)", gauge, some, full);
    println!();
    println!("{:<8} {:>9} {:>9}  {:<26} {}", "STATE", "MEM", "SWAP", "APP", "ID");
    println!("{}", "-".repeat(90));
    let empty = vec![];
    for a in v["apps"].as_array().unwrap_or(&empty) {
        let mut tags = Vec::new();
        if a["frozen"].as_bool().unwrap_or(false) { tags.push("PAUSED"); }
        if a["protected"].as_bool().unwrap_or(false) { tags.push("pinned"); }
        if a["flagged"].as_str() == Some("top_consumer") { tags.push("◀ hog"); }
        if !a["freezable"].as_bool().unwrap_or(false) && tags.is_empty() { tags.push("critical"); }
        let state = if tags.is_empty() { "live".to_string() } else { tags.join(",") };
        println!(
            "{:<8} {:>9} {:>9}  {:<26} {}",
            state,
            format_bytes(a["mem_bytes"].as_u64().unwrap_or(0)),
            format_bytes(a["swap_bytes"].as_u64().unwrap_or(0)),
            truncate(a["name"].as_str().unwrap_or("?"), 26),
            a["id"].as_str().unwrap_or("?"),
        );
    }
    println!();
    println!("Act with:  pressured ctl <cap|freeze|thaw|kill|protect|unprotect> <ID>");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
