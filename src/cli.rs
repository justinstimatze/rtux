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

/// What `admit` decided, separated from the doing so it can be tested.
#[derive(Debug, PartialEq)]
enum Admission {
    /// Run it, say nothing.
    Admit,
    /// Run it, but say why it was close.
    AdmitWithWarning(String),
    /// Don't run it. Carries the reason the user needs to act on.
    Refuse(String),
}

/// The admission decision.
///
/// # Fail open, always
///
/// `verdict: None` — no daemon, a stale daemon, an unparseable reply — **admits**.
/// This is the single most important line in the function and it is deliberately
/// not a refusal. "I could not ask" and "the answer is no" are different claims,
/// and a gate that conflates them refuses all work whenever it is itself broken,
/// then blames memory pressure for it. That exact bug shipped here once already
/// (render_budget defaulted a missing verdict to "full"), and it is the same shape
/// as the display-string gate and the cumulative-fault scar: a broken instrument
/// handing back a confident verdict. A gate nobody trusts gets removed within a
/// day, at which point it guards nothing.
///
/// `tight` admits too. This is a guard rail, not a nanny.
fn admission_for(verdict: Option<&str>, reason: &str, force: bool) -> Admission {
    if force {
        return Admission::Admit;
    }
    match verdict {
        Some("full") => Admission::Refuse(reason.to_string()),
        Some("tight") => Admission::AdmitWithWarning(reason.to_string()),
        // "ok", and — crucially — every unknown or absent verdict.
        _ => Admission::Admit,
    }
}

/// `pressured admit [--want MB] [--force] -- CMD [ARGS...]`
///
/// Run CMD only if the machine can afford it. **The admission-control caller** —
/// the thing `ctl budget` was built for and deliberately left unwired until an
/// incident named what to gate.
///
/// # Why this gates a launch, and not a prompt or a fan-out
///
/// Two measured incidents on 2026-07-15 say the same thing: the load is Claude
/// sessions in aggregate (one at 10.2GB; later 7 holding 10.8GB against an 11.4GB
/// ceiling), and a session costs ~1GB *only while active* — idle ones sit swapped
/// and cost nearly nothing. So:
///
/// - Gating **agent fan-out** would gate a non-cost: subagents are extra contexts
///   inside one existing process, not new processes. Measured before building.
/// - Gating a **prompt** in a live session destroys work already in flight, and
///   would fire hardest exactly when you are mid-thought.
/// - Gating a **launch** costs nothing when refused. Nothing is lost; you close
///   something and start again. It is also the only moment the machine can still
///   say no and be right.
///
/// This is what the prior art means by admission control: refuse work that doesn't
/// fit, rather than react cleverly once it doesn't. `advise_claude_sessions` is
/// NOT this — it notifies after the sessions are already running, and can be
/// ignored.
///
/// Use: alias `claude` to `pressured admit --want 1024 -- claude`.
pub fn cmd_admit(want_mb: Option<u64>, force: bool, cmd: &[String]) -> Result<()> {
    let Some((program, args)) = cmd.split_first() else {
        anyhow::bail!("admit needs a command: pressured admit --want 1024 -- claude");
    };

    let reply = if force { None } else { ask_budget(want_mb) };
    let verdict = reply.as_ref().and_then(|v| v["verdict"].as_str().map(String::from));
    let reason = reply
        .as_ref()
        .and_then(|v| v["reason"].as_str())
        .unwrap_or("")
        .to_string();

    match admission_for(verdict.as_deref(), &reason, force) {
        Admission::Refuse(why) => {
            eprintln!("\x1b[31m✗ not now — {why}\x1b[0m");
            if let Some(v) = &reply {
                let mb = |k: &str| v[k].as_u64().map(|b| b / (1024 * 1024)).unwrap_or(0);
                eprintln!(
                    "  apps {}M of {}M ceiling · {}M free · PSI {:.1}",
                    mb("app_current_bytes"),
                    mb("ceiling_bytes"),
                    mb("mem_available_bytes"),
                    v["psi_some_avg10"].as_f64().unwrap_or(0.0)
                );
            }
            // A refusal that doesn't say what to do is just an obstacle. Name the
            // things actually holding the memory, so "close one" is actionable
            // rather than a scavenger hunt.
            print_biggest(3);
            eprintln!("  Close one, or: pressured admit --force -- {program} ...");
            std::process::exit(1);
        }
        Admission::AdmitWithWarning(why) => {
            eprintln!("\x1b[33m~ tight — {why}\x1b[0m");
        }
        Admission::Admit => {}
    }

    // exec, not spawn: replace this process entirely so the caller sees the real
    // command's exit code, signals, and terminal — an alias must be invisible when
    // it admits, or it will be taken back out. exec() only ever returns on failure.
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(program).args(args).exec();
    Err(err).with_context(|| format!("exec {program}"))
}

/// Ask the daemon for a budget verdict. None on ANY failure — see admission_for
/// for why that must read as "no answer" and never as "no".
fn ask_budget(want_mb: Option<u64>) -> Option<serde_json::Value> {
    let req = serde_json::json!({ "cmd": "budget", "want_mb": want_mb });
    let mut stream = UnixStream::connect(SOCKET_PATH).ok()?;
    writeln!(stream, "{}", req).ok()?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp).ok()?;
    let v: serde_json::Value = serde_json::from_str(resp.trim()).ok()?;
    // An `ok:false` reply is a daemon that didn't understand the question (one too
    // old to know `budget`), not a refusal.
    if v["ok"].as_bool() != Some(true) {
        return None;
    }
    Some(v)
}

/// Print the biggest apps, so a refusal names its own remedy.
///
/// Deliberately does NOT filter on the reply's `freezable` flag, though the first
/// draft did and it produced a refusal that named the wrong things: it listed
/// Firefox (1.3GB) and Ollama (274MB) while 7.4GB of Claude sessions — the entire
/// reason for the refusal — went unmentioned.
///
/// The flag is not what it sounds like. `freezable` is `has_freeze &&
/// !never_freeze(..)`, and `never_freeze` excludes TERMINAL_NAMES ("tmux",
/// "vte-spawn", ...) — so every Claude session in a tmux-spawn scope reports
/// false. The DAEMON's freeze path uses `denied()`, which checks only
/// `hard_exempt`, and freezes those same sessions happily (measured 2026-07-15:
/// "Froze claude · rtux (1.6GB)" against a list reply that called it unfreezable).
/// Two different notions wearing one name.
///
/// The question here is "what is holding the memory you'd close", which is a human
/// decision, not a freeze-eligibility one. So: biggest first, whatever the flag
/// says. Everything in this list is an app; the spine is not in `apps`.
///
/// Best-effort and silent on failure: garnish on the message, never a reason the
/// gate itself misbehaves.
fn print_biggest(n: usize) {
    let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) else { return };
    if writeln!(stream, "{}", serde_json::json!({ "cmd": "list" })).is_err() {
        return;
    }
    let mut resp = String::new();
    if stream.read_to_string(&mut resp).is_err() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(resp.trim()) else { return };
    let empty = vec![];
    let apps = v["apps"].as_array().unwrap_or(&empty);
    for a in apps.iter().take(n) {
        eprintln!(
            "    {:>8}  {}",
            format_bytes(a["mem_bytes"].as_u64().unwrap_or(0)),
            a["name"].as_str().unwrap_or("?")
        );
    }
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

#[cfg(test)]
mod admit_tests {
    use super::{admission_for, Admission};

    /// THE test. Every other property of this gate is negotiable; this one is not.
    ///
    /// A gate that refuses when it cannot reach the daemon refuses ALL work every
    /// time it is itself broken — and blames memory pressure for it. That bug has
    /// already shipped in this repo once (render_budget defaulted a missing verdict
    /// to "full", rendering a stale daemon's "bad request" as a confident red
    /// refusal), and it is the same shape as the display-string gate and the
    /// cumulative-fault scar: a broken instrument handing back a confident verdict.
    #[test]
    fn no_daemon_admits_because_i_could_not_ask_is_not_no() {
        assert_eq!(admission_for(None, "", false), Admission::Admit);
    }

    /// A daemon too old to know `budget` replies {"ok":false,"msg":"bad request"}.
    /// ask_budget turns that into None, which must admit for the same reason.
    #[test]
    fn a_stale_daemon_admits_rather_than_blocking_every_launch() {
        assert_eq!(admission_for(None, "unknown variant `budget`", false), Admission::Admit);
    }

    /// An unrecognised verdict is an unknown, and unknowns admit. A future daemon
    /// inventing a fourth verdict must not silently start refusing launches.
    #[test]
    fn an_unknown_verdict_admits() {
        assert_eq!(admission_for(Some("wobbly"), "", false), Admission::Admit);
    }

    #[test]
    fn full_refuses_and_carries_the_reason_the_user_must_act_on() {
        assert_eq!(
            admission_for(Some("full"), "apps are at the ceiling", false),
            Admission::Refuse("apps are at the ceiling".into())
        );
    }

    /// Tight admits: a guard rail, not a nanny. A gate that balks at the first hint
    /// of scarcity gets aliased away within a day, at which point it guards nothing.
    #[test]
    fn tight_admits_but_says_so() {
        assert_eq!(
            admission_for(Some("tight"), "512MB headroom", false),
            Admission::AdmitWithWarning("512MB headroom".into())
        );
    }

    #[test]
    fn ok_admits_silently() {
        assert_eq!(admission_for(Some("ok"), "7.0GB headroom", false), Admission::Admit);
    }

    /// --force is the escape hatch that makes the gate tolerable to live with.
    #[test]
    fn force_overrides_even_a_full_machine() {
        assert_eq!(admission_for(Some("full"), "at the ceiling", true), Admission::Admit);
    }
}
