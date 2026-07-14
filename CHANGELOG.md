# Changelog

Notable changes to rtux. Versions follow [semver](https://semver.org); the git
tag is the source of truth (the binary reports it via `pressured --version`).

## [Unreleased]

### Fixed
- **Daemon silently never started at boot.** The unit's `After=graphical.target`
  combined with `WantedBy=multi-user.target` formed an ordering cycle (graphical
  → multi-user → rtux → graphical); systemd broke it by deleting rtux's start
  job, so pressured only ever ran when hand-started via `install.sh`. Removed the
  `After=graphical.target` ordering. Surfaced when an OOM event tore down the
  session while the daemon meant to be protecting the desktop wasn't running.

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

[Unreleased]: https://github.com/justinstimatze/rtux/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/justinstimatze/rtux/releases/tag/v0.1.0
