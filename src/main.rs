mod actions;
mod cgroup;
mod cli;
mod events;
mod guard;
mod ipc;
mod mitigate;
mod notify;
mod psi;
mod ranker;
mod trend;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::thread;
use std::time::Duration;

/// Sustained seconds of normal pressure required before thawing frozen apps.
/// PSI is a ~10s average that can rebound within seconds, so recovering on the
/// first normal tick caused thaw/re-freeze flapping (once observed thawing and
/// going critical again within 6s). This gate holds recovery until the calm is
/// real.
const RECOVER_AFTER_NORMAL_SECS: u32 = 10;
/// CPU-PSI `some.avg10` above this means the cores are contended enough that the
/// desktop/foreground would visibly lag — trip the active CPU throttle. (During
/// the 2026-07-14 load-27-on-8-cores lag it sat ~26.)
const CPU_PRESSURE_THRESHOLD: f64 = 15.0;

#[derive(Parser)]
#[command(
    name = "pressured",
    version = env!("RTUX_VERSION"),
    about = "Desktop responsiveness daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon: protect compositor, monitor PSI, send notifications
    Daemon,
    /// Show current pressure levels and top consumers
    Status,
    /// Query/command the running daemon over its control socket
    Ctl {
        /// list | history | touch | freeze | thaw | cap | uncap | kill | protect | unprotect
        action: String,
        /// app id (cgroup path from `ctl list`) — required for all actions except
        /// list/history/touch (touch always addresses the caller's own session)
        id: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Daemon => run_daemon(),
        Commands::Status => cli::cmd_status(),
        Commands::Ctl { action, id } => cli::cmd_ctl(&action, id.as_deref()),
    }
}

/// Apply (or re-apply) protection to the critical spine services, logging each
/// one the first time it actually lands (deduped via `announced`, so the 30s
/// re-assert doesn't re-announce). Failures are logged only when `verbose` — the
/// startup pass is legible, but a persistently-unprotectable service must not
/// spam the journal every 30s.
///
/// Returns nothing on purpose. This used to return "is everything protected?" and
/// the caller latched on it, running once and never again — which silently left
/// every post-re-login session unprotected (see the call site). Protection is a
/// standing obligation, not a milestone to reach and tick off.
///
/// Each service is independent: the compositor being protected is reported even
/// if audio can't be (a single `?` used to let audio's failure discard a
/// successful compositor protection — see guard::protect_critical_services).
fn protect_and_report(verbose: bool, announced: &mut HashSet<String>) {
    let report = match guard::protect_critical_services() {
        Ok(r) => r,
        Err(e) => {
            // Only total_ram enumeration can fail here now — genuinely rare.
            if verbose {
                eprintln!("  note: could not read memory info yet ({e}); retrying every 30s");
            }
            return;
        }
    };

    for svc in &report.protected {
        // First time this service lands, announce it — whether at startup or on a
        // later retry (a fresh login brings the compositor cgroup up after the
        // daemon started). Closing the ledger the old code never did.
        if announced.insert(svc.name.clone()) {
            println!(
                "  protected {} at {} (memory.min = {})",
                svc.name,
                svc.cgroup_path.display(),
                guard::format_bytes(svc.memory_min)
            );
        }
    }

    if verbose {
        for (name, why) in &report.failed {
            eprintln!("  note: {name} not protected yet ({why}); retrying every 30s until it lands");
        }
    }
}

fn run_daemon() -> Result<()> {
    // Auto-reap fire-and-forget children (notify-send, HUD launches) so this
    // long-lived daemon never accumulates zombie processes.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGCHLD,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }

    // Self-protection: lock our pages into RAM
    #[cfg(target_os = "linux")]
    {
        use std::io;
        extern "C" {
            fn mlockall(flags: i32) -> i32;
        }
        const MCL_CURRENT: i32 = 1;
        const MCL_FUTURE: i32 = 2;
        let ret = unsafe { mlockall(MCL_CURRENT | MCL_FUTURE) };
        if ret != 0 {
            let err = io::Error::last_os_error();
            eprintln!("warning: mlockall failed ({}), daemon may be swapped out under pressure", err);
            eprintln!("  hint: run with sudo or set CAP_IPC_LOCK capability");
        }
    }

    // Protect compositor and audio. If we started before the graphical session
    // exists (e.g. at boot, pre-login), the compositor cgroup isn't there yet —
    // so we keep retrying in the loop until protection actually lands.
    println!("pressured: protecting critical services...");
    let mut announced: HashSet<String> = HashSet::new();
    protect_and_report(true, &mut announced);

    // Control socket for the HUD / `ctl` client.
    ipc::spawn_server();

    // Monitor + mitigate loop
    println!("pressured: monitoring PSI (poll 1s, Ctrl+C to stop)...");
    let mut notifier = notify::Notifier::new();
    let mut mitigator = mitigate::Mitigator::new();
    let poll_interval = Duration::from_secs(1);
    let mut ticks: u64 = 0;
    let mut normal_streak: u32 = 0;
    let mut cpu_normal_streak: u32 = 0;

    loop {
        ticks += 1;
        // Re-assert spine protection every 30s — ALWAYS, not just until the first
        // success. Protection is not a one-shot: memory.min lives on the cgroup,
        // but oom_score_adj is per-PROCESS and only ever applied to the pids alive
        // at the time. A service restart, and above all a re-login (which builds a
        // whole new session tree), leaves the new processes unprotected forever,
        // because the old `!protected` guard latched true after the first success
        // and never ran again. Measured after the 2026-07-14 logout: the session's
        // dbus and pipewire were sitting at +200 — the daemon believed the spine
        // was protected while the kernel saw it as prime OOM meat. Re-applying is
        // idempotent and cheap; `announced` keeps the journal quiet.
        if ticks % 30 == 0 {
            protect_and_report(false, &mut announced);
        }

        let mem_psi = match psi::read_psi("/proc/pressure/memory") {
            Ok(r) => r,
            Err(e) => {
                // A transient read hiccup must not take the protector down.
                eprintln!("warning: reading memory PSI failed ({}); retrying", e);
                thread::sleep(poll_interval);
                continue;
            }
        };
        let level = psi::classify_pressure(&mem_psi);
        // Feed the rolling pressure history the HUD sparkline reads.
        trend::record(mem_psi.some.avg10);

        match level {
            psi::PressureLevel::Critical => {
                normal_streak = 0;
                // Closed loop: pause the biggest freezable hog, then — if freezing
                // is spent or swap is nearly full — SIGKILL the worst background
                // hog rather than let the climb reach the kernel's blind global
                // OOM killer (which took down the whole session on 2026-07-14).
                // kill_worst self-gates, so calling it every critical tick is safe.
                // Bias the kernel's global OOM killer toward the hogs FIRST. rtux
                // cannot intercept a global OOM at all, so if this climb is going to
                // end in one anyway, the kernel must already be holding a menu of
                // resumable background sessions rather than `systemd --user`.
                mitigator.bias_oom_toward_hogs();
                mitigator.escalate();
                mitigator.kill_worst();
                let apps = ranker::rank_apps().unwrap_or_default();
                notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Elevated => {
                normal_streak = 0;
                // Gently throttle the biggest hog first (reversible, low-stall),
                // nudge the user if a lot of Claude sessions have piled up, and
                // warn. Freezes/kills are held back until Critical.
                // Bias early: the OOM ranking must already be right BEFORE a climb
                // turns critical, since a global OOM gives no warning and no say.
                mitigator.bias_oom_toward_hogs();
                mitigator.throttle();
                mitigator.advise_claude_sessions();
                let apps = ranker::rank_apps().unwrap_or_default();
                notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Normal => {
                // Hysteresis: only thaw after pressure has stayed normal for a
                // sustained stretch, so a brief dip doesn't thaw an app straight
                // back into the pressure that got it frozen (see the const).
                normal_streak = normal_streak.saturating_add(1);
                if normal_streak >= RECOVER_AFTER_NORMAL_SECS {
                    mitigator.recover();
                }
            }
        }

        // CPU pressure is independent of memory pressure — a machine can have
        // idle RAM but saturated cores (many parallel builds/agents), which is
        // exactly what makes typing lag. Scrappy active throttle: demote
        // background hogs' cpu.weight while the cores are contended so the desktop
        // and focused app stay instant, and restore once it clears. The user's
        // priority is a responsive interface; background apps may slow.
        if let Ok(cpu_psi) = psi::read_psi("/proc/pressure/cpu") {
            if cpu_psi.some.avg10 > CPU_PRESSURE_THRESHOLD {
                cpu_normal_streak = 0;
                mitigator.cpu_throttle();
            } else {
                cpu_normal_streak = cpu_normal_streak.saturating_add(1);
                if cpu_normal_streak >= RECOVER_AFTER_NORMAL_SECS {
                    mitigator.cpu_recover();
                }
            }
        }

        thread::sleep(poll_interval);
    }
}
