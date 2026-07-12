# rtux — design notes

## What it's for

rtux is a UX project wearing a systems-daemon coat. The goal is not to prevent
out-of-memory kills (the kernel already does that) — it's that **the machine
always feels as powerful as it actually is.**

Perceived hardware power is almost entirely *foreground input latency*, and that
latency is an allocation *policy* outcome, not a hardware fact. A capable laptop
feels cheap and old the moment it thrashes, because the OS democratically starves
everything — including the cursor and compositor — under trivial contention. A
$400 phone feels powerful because iOS ruthlessly protects the foreground. So
"my machine feels powerful" decodes to "the system always serves my attention
first," which is pure policy — exactly the lever rtux holds.

Framed sharply: **rtux doesn't make your hardware more powerful; it stops the
software from *libeling* it.** A few browser tabs and some editor sessions is
*trivial* load; a multi-second stall is a *severe* symptom; that mismatch is the
slander. rtux (and zram) close the gap so the machine's felt power matches its
real, fine capability.

## The north star: legible responsiveness

Two words that separate rtux from the Apple model. iOS delivers responsiveness by
*hiding* the machinery — it silently kills your background apps and never
explains. rtux keeps the system responsive **and tells you what it did and why,
reversibly.** The 90s-sci-fi computer wasn't just fast; it *told you what it was
doing.* That legible-responsiveness is a place a Linux tool can exceed even the
Mac.

Legibility is not a nicety here — it's load-bearing. When the machine must
intervene, "paused background Chrome to keep you fast" converts the moment from a
**shame signal** ("my machine can't cope") into a **competence signal** ("my
machine is smart and runs itself").

## The endpoint: calibrated trust

What a good machine relationship actually feels like: *good, in control, on top
of things — able to steer, but confident enough to let it steer.* This is neither
failure mode:

- **Opaque automation** (iOS): steers well, won't hand you the wheel, hides its
  work. You feel *managed*.
- **Manual toil** (classic Linux): every knob is yours, so you must drive every
  second. You feel in control only because nothing is handled.

The target is the third thing — **power steering.** You decide direction; the
machine supplies the force and disappears. You feel strong *because* it's doing
the work.

Two structural requirements produce it:

1. **Legibility earns the confidence to delegate.** Trust can't be given, only
   shown. You let it steer because you watched it steer well, visibly, enough
   times that you stopped watching. Every autonomous action must be *witnessable*
   so trust can accrue and stay *calibrated* rather than blind or absent.
2. **An always-available instant override makes letting-go safe.** You relax into
   the autonomy because your hands are on the reins. The reserved, hotkey-summoned
   control surface isn't for emergencies — it's *permission to relax.* If you
   couldn't seize the wheel *this second*, you could never settle into the
   autonomy.

So the good feeling is *emergent*: **autonomy** (it acts) + **auditability** (it
shows its work) + **instant override** (you can seize the wheel) — computing as
*mastery with ease*, dignity rather than a fight.

## Why freeze, not kill

The prior art (`earlyoom`, `nohang`, `systemd-oomd`) all kill. Killing is only
humane if resumption is invisible — which iOS achieves via mandatory app-level
state restoration that Linux desktop apps don't have. Absent that, **freezing is
the reversible primitive**: `cgroup.freeze` pauses an app whole, keeps its state,
and thaws on recovery. You don't lose your tabs. iOS itself suspends background
apps by default — freeze-first is a validated bet, not a novelty.

The escalation ladder (throttle → freeze → kernel-OOM backstop) mirrors how a
careful system should degrade: ask nicely (`memory.high` reclaim), then pause
(`cgroup.freeze`), and only ever fall through to the kernel's killer as the last
resort it's meant to be.

Freeze alone halts a runaway's growth and CPU thrash but doesn't reclaim its
pages. So on freeze the daemon also issues `memory.reclaim` on the cgroup, which
pushes the frozen app's working set into compressed RAM (**zram**) — freeing
physical RAM, reversibly (it faults back on thaw). This is the reclaim benefit
CRIU promises, without CRIU: CRIU can't checkpoint the apps we actually freeze
(browsers/Electron hold Wayland, GPU/DRM, and live D-Bus connections it can't
serialize), whereas kernel memory-reclaim works on *any* app because it moves
pages, not process state. Verified: freezing + reclaiming a 400 MB process moved
its entire footprint to zram (`memory.current` 404 MB → 0).

## Why zram is the base layer

On this class of machine the single biggest anti-thrash win is compressed RAM
swap. macOS/iOS compress cold pages *in RAM* before ever touching slow storage;
that's why a Mac feels fine at 95% memory while stock Ubuntu face-plants. Linux
has the exact equivalent — **zram** — and Ubuntu, unlike Fedora (default since
2020), doesn't enable it. rtux governs the memory *wall* legibly; zram raises the
wall so you rarely reach it. They stack, and the installer offers zram for
exactly this reason.

Ubuntu's disk-swap-first default is *defensible for a heterogeneous fleet*
(incompressible workloads exist, zram costs RAM/CPU, hibernation needs disk swap,
one image spans 512MB VMs to huge servers). But every one of those reasons is a
*fleet* reason that evaporates when tuning a single, known, compressible-workload,
RAM-starved desktop. **A distro bets on the envelope; you tune the point.** That's
why enabling zram locally is right even though not defaulting it is also right.

## Lessons borrowed from iOS / macOS

- **Compression before disk** → zram.
- **Priority = attention/lifecycle** (iOS Jetsam bands: foreground > background >
  suspended) → attention-following (top roadmap item).
- **Suspend background apps by default** → validates freeze as the core primitive.
- **State restoration makes kills invisible** → why CRIU hibernate is the frontier.
- What we do *better*: stay **communicative**, not silent.

Caveat: desktop ≠ phone. iOS can be draconian because apps opt into a strict
lifecycle and mandatory state-saving; desktop apps expect to run freely in the
background. So the attention model must be gentler and always overridable.

## Architecture

- **daemon** (`pressured daemon`, root system service): PSI monitor loop →
  classify → mitigator (throttle/freeze/thaw), plus `guard` (compositor
  `memory.min`), plus an `ipc` control socket. Non-fatal loop (a transient PSI
  read never takes the protector down). Self-heals compositor protection if it
  starts before login.
- **control socket** (`/run/pressured.sock`, `0660` root:<user-gid>): `list` +
  `act` + `pin_self`. The one privileged surface; every client goes through it.
- **HUD** (`pressured-hud`, GTK4, separate binary behind the `hud` feature so the
  daemon stays GTK-free): a thin client of the socket. Updates in place; re-sorts
  only while the pointer is outside the window so rows never jump under the cursor.
- **zram** + `setup-zram.sh`: the base-layer companion, offered by the installer.

## Calm UX: never demand acknowledgement

A notification you must dismiss is a small betrayal of the whole thesis — it
*interrupts* to tell you the machine is handling things. So rtux's notifications
are transient and auto-expiring (normal urgency, `transient` hint): they fade on
their own. You never acknowledge anything.

This is the near-term expression of a larger direction (Weiser's *calm
technology*): information should live in the **periphery** and move to the centre
of attention only when you choose. The endpoint is that rtux communicates by
rendering state on the world — not by narrating it in a corner.

## The avant-garde direction (calm/ambient UX)

> **Roadmap — none of the following is built yet.** This section is the intended
> direction, not current behavior; it's here to explain where the design points.

The far target replaces the interruptive-symbolic register (a toast you read and
dismiss) with an ambient-embodied one (things you *feel*):

- **The vetoable ambient freeze.** Instead of freezing an app and then apologising
  with a notification, the window *slowly frosts over* (~1.5s) in your periphery.
  Attend to it — glance, mouse toward it, touch it — and the frost retreats: you
  claimed it (**consent through attention**). Ignore it and it rests, visibly
  frozen, a Resume glyph breathing. One gesture dissolves three problems: no
  notification to dismiss, no risk of freezing the window you're actually using,
  and no "is it broken?" confusion (the frost *is* the status).
- **The ambient field.** Computational "weight" (memory-heavy windows have drag
  and denser shadows), subtle color-temperature drift under pressure, quiet
  sonification — memory state perceived pre-attentively, never read.
- **Attention as the scheduler.** Foreground-following is the crude version; gaze
  is the avant-garde one — the window you look at is resident; anticipatory
  pre-warming thaws the one you're about to switch to before you arrive.

These ride on the compositor (the GNOME Shell extension), which is therefore not
just the indicator's home but the canvas for the whole ambient register.

## Prior art & influences

rtux stands on — and deliberately departs from — existing work:

- **Kill-based OOM responders** — [earlyoom](https://github.com/rfjakob/earlyoom),
  [nohang](https://github.com/hakavlad/nohang), and
  [systemd-oomd](https://www.freedesktop.org/software/systemd/man/systemd-oomd.service.html).
  rtux departs from all three: they *terminate* processes; rtux *pauses* them
  reversibly and keeps the compositor resident, not merely un-killed.
- **PSI (Pressure Stall Information)** — the kernel signal rtux acts on (by
  Johannes Weiner); see the
  [kernel PSI docs](https://docs.kernel.org/accounting/psi.html).
- **zram** — compressed RAM swap
  ([kernel docs](https://docs.kernel.org/admin-guide/blockdev/zram.html)), set up
  via [systemd/zram-generator](https://github.com/systemd/zram-generator). Fedora
  enables it by default (since F33, 2020); rtux offers the same to Ubuntu.
- **CRIU** ([criu.org](https://criu.org)) — the checkpoint/restore approach rtux
  evaluated and rejected for GUI apps, in favour of reclaim-to-zram.
- **iOS / macOS memory management** — Jetsam priority bands, in-RAM compression,
  and app state restoration; what rtux borrows (and where a desktop must differ)
  is in "Lessons borrowed from iOS / macOS" above.
- **Calm technology** — Mark Weiser & John Seely Brown's
  [framing](https://en.wikipedia.org/wiki/Calm_technology) is the north star for
  the ambient/peripheral UX direction.

## Status

**Validated in the wild (2026-07-12):** the full auto-mitigation fired under real
memory pressure — froze a 1.1 GB browser to hold the desktop responsive and said
so legibly, unprompted. The user's reaction ("oh nice") is the thesis confirmed:
a resource crunch felt like the machine's competence, not its failure. The
component pieces (freeze/thaw, protection, OOM immunity, throttle, socket, HUD,
pin, tray) were individually verified earlier; the PSI-Critical→freeze trip is
now proven in production.
