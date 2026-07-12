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
use std::thread;
use std::time::Duration;

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
        /// list | freeze | thaw | cap | uncap | kill | protect | unprotect
        action: String,
        /// app id (cgroup path from `ctl list`) — required for all actions except list
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

/// Attempt compositor + audio protection. Returns true if anything was protected.
/// `verbose` prints the per-service detail on the first (startup) attempt; quiet
/// retries only speak up when they finally succeed.
fn protect_and_report(verbose: bool) -> bool {
    match guard::protect_critical_services() {
        Ok(protected) => {
            if protected.is_empty() {
                if verbose {
                    eprintln!("  warning: no compositor cgroup found yet -- will retry");
                }
                false
            } else {
                for svc in &protected {
                    println!(
                        "  protected {} at {} (memory.min = {})",
                        svc.name,
                        svc.cgroup_path.display(),
                        guard::format_bytes(svc.memory_min)
                    );
                }
                true
            }
        }
        Err(e) => {
            if verbose {
                eprintln!("  warning: could not protect services: {}", e);
                eprintln!("  hint: daemon needs write access to cgroup memory.min files");
            }
            false
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
    let mut protected = protect_and_report(true);

    // Control socket for the HUD / `ctl` client.
    ipc::spawn_server();

    // Monitor + mitigate loop
    println!("pressured: monitoring PSI (poll 1s, Ctrl+C to stop)...");
    let mut notifier = notify::Notifier::new();
    let mut mitigator = mitigate::Mitigator::new();
    let poll_interval = Duration::from_secs(1);
    let mut ticks: u64 = 0;

    loop {
        ticks += 1;
        // Retry compositor protection every 30s until it succeeds (login may
        // happen after the daemon starts).
        if !protected && ticks % 30 == 0 {
            protected = protect_and_report(false);
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
                // Closed loop: pause the biggest freezable hog, then notify.
                mitigator.escalate();
                let apps = ranker::rank_apps().unwrap_or_default();
                notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Elevated => {
                // Gently throttle the biggest hog first (reversible, low-stall),
                // and warn. Freezes are held back until Critical.
                mitigator.throttle();
                let apps = ranker::rank_apps().unwrap_or_default();
                notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Normal => {
                mitigator.recover();
            }
        }

        thread::sleep(poll_interval);
    }
}
