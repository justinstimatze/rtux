# Changelog

Notable changes to rtux. Versions follow [semver](https://semver.org); the git
tag is the source of truth (the binary reports it via `pressured --version`).

## [Unreleased]

### Fixed
- **Every session was named a bare "claude" — a missing capability, not a naming
  bug.** Paused/killed sessions lost the working directory that makes them
  identifiable ("claude · rtux" → "claude"), so a kill notice named a session the
  user had no way to place or resume. Root cause: `/proc/<pid>/cwd` is
  ptrace-gated, and `ptrace_may_access()` only skips its capability check when the
  reader's creds *match* the target's — so the uid-0 daemon reading a uid-1000
  process needs `CAP_SYS_PTRACE` and otherwise gets EACCES. The unit's
  `CapabilityBoundingSet` never granted it (being root is not sufficient), while
  the world-readable `/proc/<pid>/comm` kept resolving — which is why this
  presented as a cosmetic naming regression rather than a permissions failure.
  Adding `CAP_SYS_PTRACE` (to read, never to trace) restores directory-qualified
  labels in the HUD, the journal, and kill/pause notifications. Verify with
  `scripts/install-verify-naming.sh`.

### Added
- **`pressured ctl history`.** The terminal counterpart to the HUD's activity
  strip: "what did rtux just do to my machine?" answered without opening the HUD
  — `Paused claude · rtux`, `Reclaimed 1.9GB …`, `Resumed …`, newest-first with
  relative ages. Reuses the existing `list` reply's event ring, so it adds **no
  new control-socket surface**. The ring is in-memory and resets with the daemon;
  the journal (`journalctl -u rtux.service`) remains the durable record.
- **A kill now records the swap level that justified it** — `Killed X (swap 91%)`
  in both the journal and `ctl history`, making the `SWAP_HIGH_WATER` precipice
  (the sole gate on the destructive rung) auditable in the wild instead of a
  number only the source knows.
- **`scripts/pressure-test.sh`** — an on-demand pressure harness, so the
  mitigation ladder can be *exercised* rather than waited for. Runs a memory hog
  in a transient user scope (rtux treats it as an ordinary app), ramps in steps
  while printing PSI/swap, holds at peak, then releases. It self-aborts below a
  `MemAvailable` floor, so the test can never become the out-of-memory crash it
  exists to rehearse.

### Added
- **CPU protection (passive weight reservation).** `memory.min` kept the
  compositor *resident*, but nothing reserved it CPU *time* — so under CPU
  oversubscription (a pile of parallel builds/agents; load ≫ cores) the
  compositor and the window you're typing in waited in the run queue and input
  lagged by a fraction of a second, which `memory.min` can't touch. rtux now
  enables the cgroup `cpu` controller in the session subtree and raises
  `cpu.weight` on the desktop slice (session.slice, so it out-prioritises
  app-slice bulk work) and on the **foreground** app (via the same
  attention-following path that pins its memory), resetting the boost when focus
  moves. Weights are work-conserving, so this costs nothing at idle — it only
  claims CPU when the protected thing actually needs it. (Active throttling of
  CPU hogs under CPU-PSI is a separate, more aggressive tier, not yet shipped.)

## [0.2.1] — surviving the global OOM

A full session crash on 2026-07-14 (RAM and swap both exhausted → the kernel's
own global OOM killer fired and took down dbus, collapsing the desktop) drove
this release: rtux now escalates all the way to killing background hogs before
the kernel can, protects the session bus, and biases the kernel OOM killer away
from the session spine. Plus the fixes that were sitting unreleased.

### Fixed
- **Every protection falsely reported failure (page-alignment).** The kernel
  stores `memory.min` rounded *down* to a page (4 KB) multiple, so an unaligned
  target like `total_ram/100` was stored a few bytes short and the verifying
  read-back `got >= value` never held. `set_protection` then returned `Err` for
  every service on every pass — so the daemon announced nothing, never marked
  protection "landed", and retried forever — even though the protection *was*
  applied. Targets are now aligned down to a page before writing, and the failure
  message reports the actual read-back so a real failure isn't blind.
- **A failed audio protection silently masked a successful compositor
  protection.** `protect_critical_services` protected the compositor, then the
  audio service, with `?` on each — so when the audio branch errored (its cgroup
  lookup/write failing), the whole routine returned `Err` *after* the
  compositor's `memory.min` was already written. The daemon then reported
  "compositor not protected yet" forever and retried every 30s in silence, while
  the compositor was in fact protected the whole time. Each critical service is
  now attempted independently: the compositor (the load-bearing one for
  responsiveness) is reported protected regardless of audio's fate, the retry
  stops once everything critical is secured, and a service that can't be
  protected is now logged with its reason instead of vanishing. Found while
  reinstalling the v0.2.0 daemon — the production service had been running the
  pre-v0.2.0 binary.

### Added
- **A kill rung — rtux now stops the climb to global OOM instead of watching
  it.** Its ladder topped out at "freeze," and it blanket-exempted every
  terminal (`vte-spawn`), so on a machine whose pressure comes from background
  terminal/Claude sessions it froze what little it could (a browser) and then
  sat helpless while memory climbed to the kernel's global OOM killer. Now:
  background terminal sessions are actionable (the **foreground** terminal and
  all its tabs are still spared, via focus tracking + process-descendant
  checks); and when freezing is spent or swap is ≥85% full, rtux SIGKILLs the
  worst background hog. Victim ranking is **B→C→A**: a non-Claude hog (a browser)
  dies before any Claude session, and a Claude kill is announced at critical
  urgency **with its directory** (e.g. "claude (rtux)") so the session can be
  resumed. Capped per episode; the hard-exempt spine is never touched. Also adds
  an early advisory when ≳4 Claude sessions pile up — a gentle nudge to close
  some before pressure forces anything.
- **The session bus and the kernel OOM killer are now handled** — hardening after
  a full session crash on 2026-07-14, where RAM *and* swap were exhausted, the
  kernel's *global* OOM killer fired (bypassing both rtux and systemd-oomd), and
  it killed `dbus.service` — collapsing the whole graphical session. Two gaps:
  (1) the session message bus was unprotected (`memory.min=0`), so it now joins
  the protected spine (compositor + audio + bus); (2) `memory.min` and the
  oomd-avoid xattr don't influence the *kernel* global OOM killer at all — only
  per-process `oom_score_adj` does — so rtux now writes `oom_score_adj=-1000` to
  the spine's processes. On the crashed machine dbus sat at +200 (a prime victim)
  while the memory hogs self-protected at -1000; matching the spine to -1000 lets
  the kernel fall back to size and kill the *largest* protected process (a hog)
  rather than tiny dbus. Applied only to services that don't fork the hogs, so
  the protection can't leak to them by inheritance.
- **Interventions are now witnessable under Do-Not-Disturb** (gh #1). When rtux
  acts under memory pressure while GNOME DND is on, the banner is suppressed and
  rtux's `transient` hint left nothing in the drawer either — so a freeze/reclaim
  happened with zero witness. Two fixes: the top-bar dot now *latches* a ringed
  `◉` (with a soft amber glow) the first time pressure goes critical and holds it
  — even after pressure clears — until the user opens the HUD; and the freeze
  notice drops its `transient` hint so it persists in the notification drawer
  when the banner is suppressed. The gentler rising-pressure notices stay
  transient and still fade.

## [0.2.0] — the post-crash release

Everything below was proven end-to-end under real memory pressure on
2026-07-13: the throttle → freeze → reclaim-to-zram → recover ladder fired, the
compositor stayed protected (oomd never touched the session), and the
notifications landed. Also removes the redundant tray indicator and hardens the
unit.

### Fixed
- **Daemon silently never started at boot.** The unit's `After=graphical.target`
  combined with `WantedBy=multi-user.target` formed an ordering cycle (graphical
  → multi-user → rtux → graphical); systemd broke it by deleting rtux's start
  job, so pressured only ever ran when hand-started via `install.sh`. Removed the
  `After=graphical.target` ordering. Surfaced when an OOM event tore down the
  session while the daemon meant to be protecting the desktop wasn't running.
- **oomd could still kill a protected cgroup.** `memory.min` only fends off
  kernel reclaim, not systemd-oomd's pressure-kill — so oomd could (and did)
  SIGKILL the compositor cgroup pressured was guarding. The daemon now also sets
  the `user.oomd_avoid` xattr (oomd's ManagedOOMPreference=avoid) on every cgroup
  it protects, so oomd picks it only as a last resort. Belt-and-suspenders with
  the 80% threshold drop-in.
- **Notifications never reached the screen.** As a root daemon it invoked
  `notify-send` via `runuser`, but runuser's PAM session reset the environment
  and wiped the session-bus vars, so every notice died with "Could not connect:
  Permission denied" — the user saw nothing during the very event rtux was
  handling. The bus env is now injected *inside* the target with `env`, immune to
  the reset.
- **Thaw/re-freeze flapping.** Frozen apps were thawed the instant pressure
  touched normal, but PSI is a ~10s average that can rebound in seconds (once
  thawed and went critical again within 6s). Recovery now waits for a sustained
  stretch of normal pressure before thawing.
- Softened the startup "could not protect services" message — it's a
  self-correcting cgroup-settling race (the loop retries every 30s), not the
  permissions alarm the old wording implied.

### Removed
- **Tray indicator** (`pressured-tray`) and its `ksni` dependency. It duplicated
  the GNOME Shell extension's job — a PSI-coloured top-bar dot that opens the HUD
  on click — so a standard install (installer + `install-extension.sh`) showed
  *two* dots. The extension is strictly more capable (it also does
  attention-following, which a StatusNotifierItem can't), so the tray is retired
  in its favour. `uninstall.sh` still removes any previously installed tray
  binary and autostart entry.

### Added
- **systemd-oomd reconciliation** (`50-pressured-oomd.conf`, installed to
  `/etc/systemd/system/user@.service.d/`). Ubuntu's stock oomd policy SIGKILLs
  the largest cgroup in the user slice at 50% PSI pressure — the same band
  pressured works in — so the two race and oomd wins (it can SIGKILL the
  compositor's cgroup, tearing down the session while pressured is
  mid-mitigation). The drop-in raises oomd's threshold to
  80%, ceding the 50–80% band to pressured while keeping oomd as a hard backstop.
  Removed by `uninstall.sh`.

### Changed
- Harden the systemd unit: `NoNewPrivileges`, a minimal `CapabilityBoundingSet`
  (only IPC_LOCK, SYS_RESOURCE, SETUID/SETGID, CHOWN, DAC_OVERRIDE/READ_SEARCH),
  `ProtectSystem=full`, `MemoryDenyWriteExecute`, `RestrictAddressFamilies`,
  `SystemCallFilter=@system-service` (EPERM, not kill), and the usual
  Protect*/Restrict* directives. The confinement that would break the daemon's
  job (cgroupfs writes, `/proc` scanning, dropping to the user for notifications)
  is deliberately left off and documented in the unit.

## [0.1.0] — first tagged release

The initial public cut: a working, validated desktop-responsiveness daemon.

### Added
- **Daemon**: PSI-driven `throttle → freeze → auto-recover` ladder; compositor +
  audio `memory.min` protection with a single reversible registry; runs with
  `OOMScoreAdjust=-1000` and `mlockall`; self-heals compositor protection if it
  starts before login.
- **Reclaim-to-zram**: on freeze, pushes the paused app's working set into
  compressed RAM and reports how much it moved.
- **Control socket** (`/run/pressured.sock`) with a hardened, allowlist-gated
  action surface; `pressured ctl` and a GTK4 + libadwaita **HUD** (status line,
  activity trail, pressure sparkline, per-app meters).
- **Tray indicator** and a **GNOME Shell extension** (top-bar pressure light +
  attention-following).
- **zram** base-layer setup, offered by the installer.
- Installer, uninstaller, hotkey and extension setup scripts; a contained latency
  benchmark under `benchmarks/`.

[Unreleased]: https://github.com/justinstimatze/rtux/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/justinstimatze/rtux/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/justinstimatze/rtux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/justinstimatze/rtux/releases/tag/v0.1.0
