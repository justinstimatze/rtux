# Changelog

Notable changes to rtux. Versions follow [semver](https://semver.org); the git
tag is the source of truth (the binary reports it via `pressured --version`).

## [Unreleased]

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
