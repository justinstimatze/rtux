mod actions;
mod cgroup;
mod classify;
mod cli;
mod events;
mod guard;
mod health;
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
    /// Run a command only if the machine can afford it — admission control.
    ///
    /// The gate `ctl budget` was built for. Refusing a *launch* costs nothing (you
    /// close something and start again); refusing a prompt mid-session destroys
    /// work in flight, and gating agent fan-out gates a non-cost. See cli::cmd_admit.
    ///
    /// Fails OPEN: no daemon, or a daemon too old to answer, means the command
    /// runs. "I couldn't ask" is not "no".
    ///
    /// Use: alias claude='pressured admit --want 1024 -- claude'
    Admit {
        /// Megabytes the command is expected to need (a Claude session: ~1024).
        #[arg(long)]
        want: Option<u64>,
        /// Run regardless of the verdict.
        #[arg(long)]
        force: bool,
        /// The command to run, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Query/command the running daemon over its control socket
    Ctl {
        /// list | history | budget | touch | freeze | thaw | cap | uncap | kill | protect | unprotect
        action: String,
        /// app id (cgroup path from `ctl list`) — required for all actions except
        /// list/history/budget/touch (touch always addresses the caller's own
        /// session). For `budget` this slot is megabytes, not an id:
        /// `ctl budget 2048` exits 0 if the machine can afford 2GB more, 1 if not.
        id: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Daemon => run_daemon(),
        Commands::Status => cli::cmd_status(),
        Commands::Admit { want, force, cmd } => cli::cmd_admit(want, force, &cmd),
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

/// The daemon's standing state, carried across reconcile ticks. One place instead
/// of a fistful of `let mut` locals threaded through the loop — and the seam the
/// controller's later phases hang off (each becomes a step inside `reconcile`).
struct Daemon {
    notifier: notify::Notifier,
    mitigator: mitigate::Mitigator,
    /// The instrument: the spine's major-fault rate, the outcome metric. Seeded at
    /// construction so its first tick reports a delta, not a lifetime scar.
    meter: health::FaultMeter,
    /// Spine services already announced as protected — keeps the 30s re-assert quiet.
    announced: HashSet<String>,
    ticks: u64,
    /// Consecutive normal-pressure ticks, for thaw hysteresis (see the const).
    normal_streak: u32,
}

impl Daemon {
    fn new() -> Self {
        Self {
            notifier: notify::Notifier::new(),
            mitigator: mitigate::Mitigator::new(),
            meter: health::FaultMeter::new(),
            announced: HashSet::new(),
            ticks: 0,
            normal_streak: 0,
        }
    }

    /// One turn of the control loop: **observe** the machine, **classify** the
    /// pressure, **actuate**. The caller sleeps between turns. Structured as the
    /// three reconcile phases so each later controller phase has an obvious home,
    /// but the behaviour is exactly the two-cadence loop this replaced.
    fn reconcile(&mut self) {
        self.ticks += 1;

        // ── ACTUATE (standing) ────────────────────────────────────────────────
        // Re-assert the class-driven protections every 30s — ALWAYS, not just
        // until the first success. Protection is not a one-shot: memory.min lives
        // on the cgroup and the CPU effector's standing weights (guard) persist,
        // but oom_score_adj is per-PROCESS and only ever applied to the pids alive
        // at the time. A service restart, and above all a re-login (which builds a
        // whole new session tree), leaves the new processes unprotected forever —
        // the old `!protected` latch ran once and never again. Measured after the
        // 2026-07-14 logout: the session's dbus and pipewire sat at +200 while the
        // daemon believed the spine protected. Re-applying is idempotent and cheap;
        // `announced` keeps the journal quiet.
        if self.ticks % 30 == 0 {
            protect_and_report(false, &mut self.announced);
            // Re-enumerate on the same cadence and for the same reason: a re-login
            // builds a new session tree, and a meter holding the dead session's
            // paths would report a flatlined-because-gone spine as a healthy one.
            self.meter.refresh_spine();
        }

        // ── OBSERVE ───────────────────────────────────────────────────────────
        // Sample the spine's fault rate BEFORE reading PSI and deciding what to do.
        // This is the outcome metric — whether the interactive path is waiting on
        // disk right now — and it is measured unconditionally, at every pressure
        // level, because the harm we care about does not announce itself via PSI
        // first. The 19s stall ran at cpu PSI 2.28 and memory PSI no threshold here
        // would have called critical; what it had was the input method faulting on
        // every keypress. That is what this counts.
        self.meter.tick();

        let mem_psi = match psi::read_psi("/proc/pressure/memory") {
            Ok(r) => r,
            Err(e) => {
                // A transient read hiccup must not take the protector down. The
                // caller still sleeps this tick, so cadence is unchanged.
                eprintln!("warning: reading memory PSI failed ({}); retrying", e);
                return;
            }
        };
        let level = psi::classify_pressure(&mem_psi);
        // Feed the rolling pressure history the HUD sparkline reads.
        trend::record(mem_psi.some.avg10);

        // ── ACTUATE (reactive): the pressure ladder ───────────────────────────
        match level {
            psi::PressureLevel::Critical => {
                self.normal_streak = 0;
                // Closed loop: pause the biggest freezable hog, then — if freezing
                // is spent or swap is nearly full — SIGKILL the worst background
                // hog rather than let the climb reach the kernel's blind global
                // OOM killer (which took down the whole session on 2026-07-14).
                // kill_worst self-gates, so calling it every critical tick is safe.
                // Bias the kernel's global OOM killer toward the hogs FIRST. rtux
                // cannot intercept a global OOM at all, so if this climb is going to
                // end in one anyway, the kernel must already be holding a menu of
                // resumable background sessions rather than `systemd --user`.
                self.mitigator.bias_oom_toward_hogs();
                self.mitigator.escalate();
                self.mitigator.kill_worst();
                let apps = ranker::rank_apps().unwrap_or_default();
                self.notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Elevated => {
                self.normal_streak = 0;
                // Gently throttle the biggest hog first (reversible, low-stall),
                // nudge the user if a lot of Claude sessions have piled up, and
                // warn. Freezes/kills are held back until Critical.
                // Bias early: the OOM ranking must already be right BEFORE a climb
                // turns critical, since a global OOM gives no warning and no say.
                self.mitigator.bias_oom_toward_hogs();
                self.mitigator.throttle();
                self.mitigator.advise_claude_sessions();
                let apps = ranker::rank_apps().unwrap_or_default();
                self.notifier.maybe_notify(level, &apps);
            }
            psi::PressureLevel::Normal => {
                // Hysteresis: only thaw after pressure has stayed normal for a
                // sustained stretch, so a brief dip doesn't thaw an app straight
                // back into the pressure that got it frozen (see the const).
                self.normal_streak = self.normal_streak.saturating_add(1);
                if self.normal_streak >= RECOVER_AFTER_NORMAL_SECS {
                    self.mitigator.recover();
                }
            }
        }

        // NOTE: there is deliberately no reactive CPU rung here, and re-adding one
        // would be a regression. A previous version demoted background hogs'
        // cpu.weight whenever CPU PSI crossed a threshold, on the theory that
        // saturated cores are "exactly what makes typing lag". Measurement on
        // 2026-07-14, during a real ~19s keyboard stall, says otherwise:
        //
        //     cpu PSI some.avg10 =  2.28     <-- cores essentially idle
        //     io  PSI some.avg10 = 34.82     <-- the actual stall
        //
        // Typing lagged because the input method was on a disk swapfile, so every
        // keypress took a major fault. The cores were never the problem, and the
        // throttle demoted a dozen background apps for nothing.
        //
        // It could not have worked anyway. cgroup v2's four resource models are
        // Weights / Limits / Protections / Allocations, and `cpu.weight` is a
        // Weight: a share of `w_i / Σ w_active`, where the denominator floats with
        // whatever else is runnable. There is no `cpu.min` — cgroup v2 cannot
        // express a CPU *floor* at all, so no arrangement of weights guarantees the
        // compositor anything. What survives is the standing, class-driven weight
        // boost applied in guard.rs (Guaranteed's session.slice above app.slice,
        // the Focused leaf above its siblings): cheap, work-conserving, honest
        // about being a preference rather than a guarantee, and — unlike this loop
        // — not pretending to be a floor. That is the whole CPU effector, by design.
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
    let mut daemon = Daemon::new();
    protect_and_report(true, &mut daemon.announced);

    // Control socket for the HUD / `ctl` client.
    ipc::spawn_server();

    // The reconcile loop: observe → classify → actuate, once a second. All the
    // standing state lives in `Daemon`; the body of a turn is `Daemon::reconcile`.
    println!("pressured: monitoring PSI (poll 1s, Ctrl+C to stop)...");
    let poll_interval = Duration::from_secs(1);
    loop {
        daemon.reconcile();
        thread::sleep(poll_interval);
    }
}
