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
    // `history` is a read-only view of the same `list` reply (its `recent`
    // field) — no separate daemon endpoint, so no extra control-socket surface.
    let req = match action {
        "list" | "history" => serde_json::json!({ "cmd": "list" }),
        // Self-only: the daemon marks the CALLER's cgroup live via SO_PEERCRED, so
        // this deliberately sends no id — there is nothing to address but yourself.
        "touch" => serde_json::json!({ "cmd": "touch" }),
        // `budget [MB]` — the id slot carries megabytes, not a cgroup. Read-only.
        "budget" => {
            let want_mb = match id {
                Some(s) => Some(s.parse::<u64>().with_context(|| {
                    format!("`ctl budget` takes megabytes, not {s:?} (e.g. `ctl budget 2048`)")
                })?),
                None => None,
            };
            serde_json::json!({ "cmd": "budget", "want_mb": want_mb })
        }
        _ => {
            let id =
                id.context("this action needs an app id (get one from `pressured ctl list`)")?;
            serde_json::json!({ "cmd": "act", "action": action, "id": id })
        }
    };

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        // A gate must never mistake "I couldn't ask" for "the answer is no". See
        // render_budget for why this is exit 2 and not an error like everything else.
        Err(e) if action == "budget" => {
            eprintln!("? no verdict — cannot reach the daemon at {SOCKET_PATH} ({e})");
            eprintln!("  This is NOT a refusal. Decide without rtux.");
            std::process::exit(2);
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("connecting to {} (is the daemon running?)", SOCKET_PATH))
        }
    };
    writeln!(stream, "{}", req)?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let v: serde_json::Value =
        serde_json::from_str(resp.trim()).context("parsing daemon reply")?;

    if action == "list" {
        render_hud(&v);
    } else if action == "history" {
        render_history(&v);
    } else if action == "budget" {
        return render_budget(&v);
    } else {
        let ok = v["ok"].as_bool().unwrap_or(false);
        println!("{} {}", if ok { "✓" } else { "✗" }, v["msg"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// Render a budget verdict and — the whole point — set the exit code, so a gate
/// can be `pressured ctl budget 2048 || refuse` with no JSON parsing at all.
///
///   0 = admitted (ok or tight)
///   1 = refused (full)
///   2 = NO VERDICT — the daemon couldn't be reached, is too old to know `budget`,
///       or answered something unparseable.
///
/// 2 exists because "I have no answer" and "the answer is no" are different claims
/// and must not share an exit code. The first draft of this function did share one:
/// it defaulted a missing `verdict` field to "full", so an older daemon replying
/// `{"ok":false,"msg":"bad request"}` rendered as a confident red refusal with an
/// empty reason. A gate wired to that would have refused all work whenever the
/// daemon was stale — and blamed memory pressure for it. This is the same failure
/// mode as the display-string gate and the stale journal (see DESIGN.md): a broken
/// tool handing back a confident verdict. Fail loud, never fail decisive.
///
/// "tight" admits on purpose: this is a guard rail, not a nanny, and a gate that
/// refuses at the first hint of scarcity gets turned off within a day — at which
/// point it guards nothing. Callers wanting the finer distinction read `verdict`
/// off the socket directly.
fn render_budget(v: &serde_json::Value) -> Result<()> {
    let answered = v["ok"].as_bool().unwrap_or(false);
    match v["verdict"].as_str() {
        Some(_) if answered => {}
        _ => {
            let msg = v["msg"].as_str().unwrap_or("no verdict field in the reply");
            eprintln!("? no verdict — the daemon did not answer the budget question ({msg})");
            eprintln!("  Most likely it predates `ctl budget`; try `sudo ./install.sh`.");
            eprintln!("  This is NOT a refusal. Decide without rtux.");
            std::process::exit(2);
        }
    }
    let verdict = v["verdict"].as_str().unwrap_or("full");
    let reason = v["reason"].as_str().unwrap_or("");
    let (mark, colour) = match verdict {
        "ok" => ("✓", "\x1b[32m"),
        "tight" => ("~", "\x1b[33m"),
        _ => ("✗", "\x1b[31m"),
    };
    println!("{colour}{mark} {verdict}\x1b[0m — {reason}");

    let mb = |k: &str| v[k].as_u64().map(|b| b / (1024 * 1024));
    if let (Some(cur), Some(head)) = (mb("app_current_bytes"), mb("headroom_bytes")) {
        let ceiling = mb("ceiling_bytes").map(|c| format!("{c}M")).unwrap_or_else(|| "unset".into());
        println!(
            "  apps {cur}M of {ceiling} ceiling · {head}M headroom · {}M free · PSI {:.1}",
            mb("mem_available_bytes").unwrap_or(0),
            v["psi_some_avg10"].as_f64().unwrap_or(0.0)
        );
    }

    if verdict == "full" {
        std::process::exit(1);
    }
    Ok(())
}

/// Render just the recent-actions trail — "what did rtux do lately" — from a
/// `list` reply's `recent` field, newest-first. The terminal counterpart to the
/// HUD's activity strip: answers "did rtux touch my session?" without opening
/// the HUD. This is an in-memory ring (capped, reset when the daemon restarts);
/// the systemd journal (`journalctl -u rtux.service`) is the durable record.
fn render_history(v: &serde_json::Value) {
    let empty = vec![];
    let events = v["recent"].as_array().unwrap_or(&empty);
    if events.is_empty() {
        println!("No recent interventions — rtux has been quiet.");
        return;
    }
    println!("Recent rtux interventions (newest first):");
    println!();
    for e in events {
        let ago = e["ago_secs"].as_u64().unwrap_or(0);
        let text = e["text"].as_str().unwrap_or("?");
        println!("  {:>6} ago   {}", fmt_ago(ago), text);
    }
}

/// Compact relative age: `45s`, `12m`, `2h3m`.
fn fmt_ago(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
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
    render_spine_health(v);
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

/// The guarantee, as a number: is the interactive path waiting on disk?
///
/// This is the line the HUD was missing. Everything else it shows — pressure,
/// swap, who's big — is a proxy or a cause; this is the outcome. rtux's whole
/// claim is "the spine stays resident", and until now nothing on screen said
/// whether that was true.
///
/// Both numbers are rates, never totals — see health.rs for why the total is a
/// scar that never heals and would sit here permanently red.
///
/// The peak is shown only when it disagrees with the instant, because that is the
/// only time it adds anything: a stall lasts a second or two and is long over by
/// the time a human opens the HUD, so "0 now" alone would report every incident
/// as a clean bill of health.
///
/// Three states, not two. "Unknown" is a real answer and gets said out loud —
/// a meter that found no spine to read must not render as a green clean bill,
/// which is what summing an empty set silently produces.
fn render_spine_health(v: &serde_json::Value) {
    // Absent entirely means an older daemon that predates the meter. Say nothing
    // rather than print a verdict it never gave us.
    if v.get("spine_observed").is_none() {
        return;
    }
    let worst = v["spine_worst"].as_str();
    let line = match (v["spine_faults_now"].as_u64(), v["spine_faults_peak"].as_u64()) {
        (Some(0), Some(0)) => {
            "\x1b[32mSpine: resident — 0 major faults/s (clean for the last minute)\x1b[0m".to_string()
        }
        (Some(0), Some(p)) => format!(
            "\x1b[33mSpine: resident now — 0 major faults/s, but peaked at {p}/s in the last minute\x1b[0m"
        ),
        (Some(n), _) if n > 0 => format!(
            "\x1b[31mSpine: WAITING ON DISK — {n} major faults/s{}\x1b[0m",
            worst.map(|w| format!(" (worst: {w})")).unwrap_or_default()
        ),
        // observed == 0: the meter is looking at nothing. Either the session tree
        // moved or the spine genuinely isn't there — both mean rtux is guarding
        // something it can't find, which is worse news than any fault rate.
        _ => "\x1b[31mSpine: UNKNOWN — rtux found no spine cgroups to measure\x1b[0m".to_string(),
    };
    println!("{line}");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
