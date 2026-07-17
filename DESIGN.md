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

### QoS-class lineage (the controller's ancestors)

The unified controller is not a new idea; it is the desktop retrofit of a design
three other ecosystems already ship. Each *declares* the class; rtux must infer it.

- **macOS / Darwin QoS classes** — user-interactive → user-initiated → utility →
  background, driving CPU priority, core placement, and (via App Nap) background
  disk/network-IO throttling and timer coalescing. The closest match to "protect the
  focused workload across CPU+IO," but cooperative — apps declare their class.
  [Apple: App Nap](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/AppNap.html),
  [eclecticlight: macOS QoS](https://eclecticlight.co/2022/01/07/how-macos-controls-performance-qos-on-intel-and-m1-processors/).
- **Android top-app cpuset** — the visible app gets its own fast core plus a ~10%
  schedtune boost; background is packed onto the slow cores. Focus-following in
  shipped, billion-device form, built from the same cgroup primitives rtux uses —
  but Android's framework *knows* the foreground app.
  [AOSP: performance management](https://source.android.com/docs/core/power/performance),
  [LWN: scheduling for Android](https://lwn.net/Articles/706374/).
- **Windows MMCSS** — reserves a percentage of CPU for background work
  (`SystemResponsiveness`), i.e. a userspace CPU *floor* where cgroup v2 has none —
  and the cautionary tale: too rigid a reservation *causes* the audio glitch it was
  meant to prevent. Keeps rtux's floors adaptive.
  [Microsoft: MMCSS](https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service).
- **BeOS pervasive multithreading** — the responsiveness legend, and the instructive
  *mis*-match: it made everything responsive by making threads cheap (one per
  window), not by deciding who loses under scarcity. It has no memory-overcommit
  story; rtux lives in the regime BeOS's approach runs out in.
  [OSnews](https://www.osnews.com/story/180/making-the-case-for-beoss-pervasive-multithreading/),
  [LWN](https://lwn.net/Articles/495229/).
- **Kubernetes QoS** — Guaranteed / Burstable / BestEffort, per-workload requests and
  limits, admission control, pressure eviction by class. The cloud vocabulary rtux's
  design maps onto one-for-one — except the two assumptions that don't survive a
  desktop: workloads are fungible/replaceable (yours hold irreplaceable state) and
  scaling is horizontal (there are no replicas; the reversible *pause* is the
  substitute for stateless eviction, and it is the better tool).

## Standing guarantees, not reactive rungs

The organizing principle, arrived at the hard way (see the postmortem below). A
*standing* guarantee is config written once and left in force: the spine's
`memory.min`, `swap.max=0`, `oom_score_adj`, the `app.slice` ceiling, the CPU
weight boost, swap fault-in. A *reactive* rung is something the PSI loop does
after pressure arrives: freeze, reclaim, kill.

Standing guarantees are the product. Reactive rungs are the backstop, and the
backstop firing at all is a partial failure of the standing layer. PSI is a ~10s
average — by the time it crosses a threshold the user has already felt the stall,
so anything that only *starts working* at that point is late by construction. The
whole reactive ladder is what remains after the standing layer has been outrun; it
is not the mechanism by which the desktop stays fast.

The practical test for any new lever: does it hold a guarantee *before* trouble,
or does it react *to* trouble? Prefer the first. Add the second only as a
last resort, and only when it can't be expressed as the first.

## Postmortem: the 2026-07-14 incident

Two failures in one day — a kernel global OOM that killed `systemd --user` (taking
the session with it), and ~19s of keyboard latency. What the cleanup established,
recorded here so none of it gets rediscovered or silently re-added:

- **cgroup v2 has no `cpu.min`.** Its four resource models are Weights / Limits /
  Protections / Allocations, and `cpu.weight` is a Weight: a share of
  `w_i / Σ w_active`, where the denominator floats with whatever else is runnable.
  There is no way to express a CPU *floor*. A reactive CPU-throttle rung once
  existed here on the theory that saturated cores cause input lag; it was deleted.
  Measured during the actual stall: `cpu` PSI `some.avg10 = 2.28` (cores idle)
  against `io` PSI `some.avg10 = 34.82`. Typing lagged because the input method was
  on a *disk* swapfile and every keypress took a major fault. The cores were never
  the problem. What survives is the standing weight boost — honest about being a
  preference rather than a guarantee.

- **`oom_badness()` scores on RSS.** Pinning a service resident without also
  biasing `oom_score_adj` makes it a *more* attractive global-OOM victim, because
  you just made it bigger. Protection and biasing must ship together or the
  protection is an accelerant. `memory.min` and the oomd `avoid` xattr do not
  influence the kernel's global killer at all; only `oom_score_adj` does.

- **`memory.swap.max=0` is prophylactic, never curative.** It forbids *future*
  swap-out. It does not recall a page that is already gone, so a service that got
  evicted before protection landed stays slow forever. Hence fault-in: walk the
  pids' swapped pages and touch them back into RAM. This is why protection has to
  be a standing obligation re-asserted on a timer, not a one-shot at startup.

- **Shmem swap cannot be faulted in by touching addresses.** An evicted shmem page
  (tmpfs, `wl_shm`, dma-buf, Xwayland pixmaps) leaves no swap PTE in the mapper —
  the shmem inode owns the slot — so `memory.swap.current` counts it while no
  process admits to it and no address touch can reach it. Fault-in handles the anon
  share, which is what the latency-critical units (input method, audio, session bus,
  the user's terminal) are made of.

  The corollary, learned by getting it wrong: **do not diagnose shmem by comparing
  `memory.stat anon` to `memory.swap.current`.** `anon` counts *resident* anonymous
  memory, so a page that swaps out leaves the counter by definition; the comparison
  is vacuous and reads as convincing anyway. The real discriminator is smaps `Swap:`
  summed over the cgroup's pids versus `memory.swap.current` — it asks the only
  question that matters: is there a swap PTE to touch?

  And the shmem gap is not damage to be repaired — it is a cache that breathes.
  Watched over minutes with RAM free, the compositor's swap ranged 551MB → 120MB →
  461MB while its resident shmem swung between roughly 0.4GB and 4.3GB: the kernel
  evicting cold client buffers and faulting back hot ones, exactly as a cache
  should. Do not read a low sample as "healed" or a high one as "damaged" — the
  first draft of this section did the former, on one sample, and called it a trend.
  The lever that matters is making room (the `app.slice` ceiling), not touching
  addresses; and what actually needs measuring here is compositor *latency*, not the
  swap counter, which is a cache statistic wearing a scary number.

- **Never let a display string be a gate.** `protect_critical_services` returned
  services whose `name` had become `"compositor (org.gnome.Shell@ubuntu.service)"`
  for logging, while two callers still did `find(|s| s.name == "compositor")`. Both
  silently matched nothing — a `find` that matches nothing is indistinguishable from
  a compositor that isn't running, so it printed no error. The CPU boost was dead
  from that commit onward, and stale cgroup values from before it made the feature
  look alive. Anything code dispatches on now lives in a separate stable `class`
  field, with a test asserting the literal exists.

- **Measure progress, not effort.** Fault-in's first version budgeted *pages
  touched*, so the compositor spent its entire budget faulting already-resident
  pages inside huge shmem VMAs and recalled nothing. It now filters candidates
  through `/proc/<pid>/pagemap` bit 62 (swapped) and budgets pages actually out.
  Relatedly, it reports what it *touched* rather than the cgroup's swap delta: the
  target is a live process the kernel is concurrently faulting in and evicting, so
  `before - after` folds the kernel's work into ours and would invent successes.

- **A broken tool is not a measurement.** Two conclusions this day were nearly drawn
  from tooling that failed silently: `sudo -n` returns nothing useful without a TTY
  (it looks like an empty result, not an error), and a verification script that
  grepped the whole journal replayed a *previous* run's verdict as current. Both
  times the tool was broken, not the daemon. Scripts under `scripts/` now pin
  themselves to the running binary and the current unit start time.

## Measure harm, not swap

`memory.swap.current` is the wrong number to optimize, and staring at it costs
real time (2026-07-14: a whole afternoon). Swap is the *goal* for app.slice —
every page an app has on disk is a page the desktop didn't have to give up. The
harm was never swapping; the harm is **a major fault on the interactive path at
the moment of interaction**. The 19s stall was not "the input method was swapped",
it was "the input method was swapped *and a key was pressed*". A cold buffer
nobody touches costs nothing.

So the metric is `pgmajfault` on the spine, not `swap.current` anywhere. Measured
9.5h after the realignment landed:

    app.slice (all apps)   15,752,391 major faults
    entire spine              ~100,000 major faults

Two orders of magnitude. That gap *is* the partition working: 15.7M times the
kernel chose to make a background job wait instead of the user. A spine
pgmajfault **rate** belongs in the HUD — it is the only honest way to see whether
the guarantee is holding, and its absence is why a benign cache statistic was
able to masquerade as an incident for an afternoon.

## The recurring bug: a confident claim computed from the wrong thing

Six times in two days, in six different subsystems, one bug. It is worth naming as a
rule rather than as six postmortems, because it will happen a seventh time.

**Every claim rtux makes — a HUD tag, a log line, a gate's verdict, a metric — must
be computed from the same predicate, at the same moment, as the behaviour it
describes. Where it cannot be, it must be able to say "I don't know."**

The roster, so the shape is unmistakable:

| the claim | computed from | the behaviour it described |
|---|---|---|
| "compositor is CPU-boosted" | `name == "compositor"` | a `name` that had become a display string |
| "the spine is protected" (script) | the whole journal | a *previous* run's verdict |
| `✗ full` (`ctl budget`) | a missing `verdict` field | a daemon too old to know the question |
| "the compositor is hurting" | `pgmajfault` **total** | a rate that was 0; the total was a scar |
| "Spine: resident — clean" | a sum over **zero** cgroups | a meter that found no spine to read |
| `critical` (HUD, i.e. protected) | `never_freeze` (client permission) | `denied()`, which freezes it anyway |

Two corollaries earn their keep:

- **An empty set sums to zero, and zero renders green.** Every signal needs an
  explicit "I couldn't look" state, distinct from "nothing is wrong". `ctl budget`
  exits **2** for no-verdict rather than folding it into refusal; `health::Sample`
  carries `observed` so a blind meter can't pose as a healthy one; `admit` **fails
  open**, because "I could not ask" is not "the answer is no".
- **Never dispatch on a string built for humans.** Display labels drift, get
  prettified, and collide — `hard_exempt` matches by substring against a list ending
  in `"rtux"`, so the label `claude · rtux` hard-exempts itself. Dispatch on a stable
  `class` field; keep the pretty name for humans and test that the literal exists.

**The tell is always the same: the failure is silent and the tool is confident.** A
`find` that matches nothing looks identical to a compositor that isn't running. An
unreachable daemon looks identical to a full machine. A scar looks identical to a
wound. None of these printed an error; every one of them printed an answer. So the
question to ask of any new signal is not "is it right?" but **"what does it say when
it's broken, and can I tell that apart from good news?"**

Corollary for reviewers: a comment stating the intent is not the intent. The
top-consumer marker's own comment read *"so the top-consumer marker never promises a
pause that won't come"* directly above the line that broke that promise.

## The controller: one QoS loop, not a bag of reflexes

*Architecture-of-record, adopted 2026-07-16. The levers below become its phases.*

Every lever rtux has — the `memory.min` floor, `cgroup.freeze`, the `cpu.weight`
boost, `pressured admit` — is a facet of one thing rtux has never named: a
**single-node QoS controller**. The desktop is a datacenter of one node; each app
is a workload; the kernel already ships every sensor and actuator (PSI, per-cgroup
`memory.current`, `memory.min`/`.high`, `cpu.weight`, `io.latency`, `cgroup.freeze`)
and then stops — on purpose — because the missing piece is *policy*, and policy needs
to know what the human cares about, which the kernel refuses to guess. rtux is that
missing policy layer. In Kubernetes terms it is not the workload; it is the
**scheduler and the kubelet**.

The reason this keeps feeling like "basic cloud shit that should already exist" is
that it does exist — three times over — just not on the Linux desktop:

- **Kubernetes** QoS classes: Guaranteed / Burstable / BestEffort, with per-workload
  *requests* (floors) and *limits* (caps), an admission controller that refuses pods
  that don't fit, and pressure eviction by class.
- **macOS / Darwin** QoS classes: user-interactive → user-initiated → utility →
  background, driving CPU priority, P-core vs E-core placement, and — via App Nap —
  disk- and network-IO throttling and timer coalescing for background apps.
- **Android** cpusets: the visible app goes in `top-app` (its own fast core + a
  ~10% schedtune boost); background is *packed* onto the slow cores.

They are the same design wearing three uniforms: **an explicit priority class per
workload, plus a controller that reserves for the top class and throttles the rest
across resources.** And each got to *assume* the class — pods declare requests, Apple
apps declare QoS, Android owns the activity lifecycle. **The Linux desktop has no app
model that declares anything**, so rtux's genuinely novel, hard part is *inferring*
the class without app cooperation. Focus is the one class signal observable from
outside the app — which is why "protect the focused workload" is not one option among
many; it is the only objective the environment lets us sense.

### The class model

Four classes, named by the precedents above, ordered by claim on the machine:

- **Guaranteed** — the spine (compositor, input method, audio, WM, the daemon).
  Hard `memory.min`, top `cpu.weight`, never evicted. Structural, from the `SPINE`
  table.
- **Focused** — the workload the user is interacting with right now (the focused
  window's cgroup, and terminals touched in the last few seconds). A `memory.min`
  bump, a `cpu.weight` boost, IO-latency protection, and immunity from eviction
  *while focused*. This is Android's `top-app`, inferred instead of declared.
- **Active** — app.slice members doing work but not focused. Under the ceiling,
  throttled and frozen under pressure, ordered worst-first.
- **Idle** — swapped-out background sessions and unattended services. `cpu.idle`,
  squeezed first, the source of reclaimed headroom.

### The classifier: fast tier + judgment tier

Assigning the class is the heart of the controller, and it splits the way "why is my
machine out of memory?" splits when a human asks it — a fast glance plus a considered
stance on what actually matters:

- **Fast tier** — a pure `classify(observation) -> Class`, every tick, in-process,
  microseconds. Inputs are all cheap: spine-membership, the focused cgroup (from the
  extension), touched-recently, `comm`/cgroup name, `memory.current`, fault rate.
  This is `spared_now` + the spine table generalised into a classifier. Deterministic
  and dependable — a root daemon's hot path must never block.
- **Judgment tier** — the considered stance, run out-of-band and *cached*. It assigns
  priors by workload identity ("a browser tab is more restorable than a video call",
  "a Claude session mid-thought is Active-not-Idle, freezing it destroys work",
  "ollama serving nobody is pure Idle"). Heuristic ruleset now; could be model-
  assisted later. It runs on first sight of an unknown app or on a slow timer, never
  on the tick, and writes verdicts the fast tier reads. A missing verdict falls back
  to structural class — the fast tier is never gated on it. (Same discipline as the
  API-cache rule: deterministic fast path, expensive judgment cached off the hot
  path.)

### The actuator map — three resources, three different strengths

The honest part the prior art forces: the three resources do **not** actuate with
equal strength, and pretending they do is the MMCSS mistake (a floor too rigid
becomes the glitch it was meant to prevent).

| Resource | Guaranteed / Focused | Active / Idle | Guarantee strength |
|---|---|---|---|
| Memory | `memory.min` (hard floor) | `memory.high` (adaptive cap) | **hard** — the floor is real |
| CPU | `cpu.weight` boost, `cpu.uclamp.min` | `cpu.idle`, `cpu.max` on the *complement* | **reserved headroom** — no `cpu.min` exists; the floor is faked by capping everyone else (MMCSS's `SystemResponsiveness`) |
| IO | `io.latency` target | `io.max` / `io.weight` throttle | **throughput fairness** — helps the fault *stream*, not a single fault in flight |

The CPU floor is the one genuine *kernel* gap: cgroup v2 has no `cpu.min`, so a hard
CPU guarantee is inexpressible. It is plugged in userspace by ceiling the Active+Idle
complement and demoting Idle to `cpu.idle`, which reserves headroom for Focused
without a kernel patch — coarse, but sufficient, and exactly what Windows does.

### The reconcile loop

One loop replaces the two independent ones (per-tick mitigate + 30s protect). Each
tick: **observe** (PSI mem/cpu/io, per-cgroup current+swap, fault rate, focused
cgroup) → **classify** (fast tier, falling back through the judgment cache) →
**actuate** (drive the effectors per the map). The existing modules become organs:
`guard` → the memory/CPU request effector; `mitigate` → the eviction effector (its
`spared_now` moves *into the classifier*, since "expendable" is a class question — the
exact confusion behind the July HUD bug); `health` → the sensor feeding the objective;
`admit` → reads the *same* class/headroom model, so admission and runtime control
finally agree instead of each computing headroom their own way.

## IO is a first-class load, and the lever is switched off

*The finding that promoted IO from a footnote to a phase, measured 2026-07-16.*

rtux's worst incidents — 19s keyboard latency, a 40s wake-from-lock — were **IO**
events wearing a memory costume. A major fault is a block read from the swap device;
`memory.min` prevents the eviction, but for the pages that *do* leave, nothing today
protects the fault-back-in latency, and that is pure IO. On rukh the swap chain is
zram (prio 100, compressed RAM, no block IO) spilling to `/swapfile` and the LVM swap
on the NVMe — so every swap page beyond zram is a block-IO event on the same device
the compositor faults through.

And IO is the *dominant active stall source* on this box right now — not memory:

    root io.pressure    some avg60=0.69  full avg60=0.59   (~71 min cumulative since boot)
    root memory PSI      some avg10=0.03

IO PSI runs ~10–20× the memory PSI. The instinct that IO is under-appreciated as a
system load is correct, and measured.

**The lever exists in the kernel and is switched off for user apps.** cgroup v2's io
controller is available at the root and enabled for `system.slice`, but the chain to
user apps drops it. Traced live on rukh (systemd 259):

    <root>            subtree_control: cpuset cpu io memory pids
    user.slice        controllers:     cpuset cpu io memory pids  <-- io is available here…
    user.slice        subtree_control: cpu memory pids            <-- …but not passed down
    user-1000.slice   controllers:     cpu memory pids             (io gone)
    user@1000.service DelegateControllers: cpu memory pids         <-- the hard wall
    app.slice         controllers:     cpu memory pids             (no io.latency to set)

So `system.slice` services already have IO control and *your apps do not*. The
decisive wall is the last line: `user@1000.service` runs with `Delegate=pids memory
cpu` (the Ubuntu vendor default, `/usr/lib/systemd/system/user@.service:28`), so the
per-user `systemd --user` instance is *structurally forbidden* from managing io on
anything it owns, `app.slice` included. This is why the first spike failed the way it
did: it reached for `IOAccounting=yes`, but you cannot account for a controller you
were never delegated — the accounting knob was the wrong lever entirely.

The right lever is `Delegate=`. A one-line drop-in on `user@.service` —

    [Service]
    Delegate=pids memory cpu io cpuset

— adds io to the delegated set, and systemd then enables io in the subtree_control of
every ancestor up the chain so it can be handed down. (`cpuset` rides along for free
and the CPU-idle effector wants it too.) rtux already ships a drop-in in exactly this
directory — `50-pressured-oomd.conf` — so the delegation override sits right beside it.

**There is no live, no-logout proof of this — and that shaped the design.** A running
`user@1000.service` realises its delegated controllers *once, at login*, and will not
re-delegate without a restart (which is a logout); the template `user@.service` exposes
no resolved `DelegateControllers` to preview. So the only ground truth is
`app.slice/cgroup.controllers` *after a fresh login* — an early spike that tried to
verify reversibly within one session was chasing something structurally unobservable.
The deployment is therefore honest about the seam it crosses: the installer lays the
drop-in, prompts for a re-login, and **the daemon capability-detects io on `app.slice`
at startup** — if it is absent, the IO effector is simply skipped. That detection is the
real safety net, not a pre-flight check, and it makes the change reversible by
construction (delete the drop-in, log back in). `scripts/io-delegation-spike.sh` applies
the drop-in to `/run` (reboot-clears) and hands you the one check that means anything —
`grep -w io …/app.slice/cgroup.controllers` after re-login — with a `--revert` to pull
it now.

**The honest limit:** `io.latency` cannot rescue a fault already in flight — that is a
residency problem `memory.min` already owns. What it protects is the fault *stream*
during recovery — waking from lock is thousands of faults, a sustained IO burst where
`io.latency` prioritises the compositor's reads over a background job's writes. So IO
control is a real new guarantee for contention and recovery, and explicitly *not* a
substitute for keeping the spine resident.

## Levers not yet pulled

*These are now the phases of the controller above.* Phase 1 extracts the classifier
(behaviour-identical); phase 2 introduces the reconcile loop and the CPU effector;
phase 3 is the per-session memory limit (#1 below); phase 4 plugs IO behind the
delegation spike; phase 5 is the judgment tier. Ranked by how much of the *felt* gap
each one closes — the distance between "rtux
reacted correctly" and "the machine felt powerful," which the 21:31 incident proved
are different things. Recorded 2026-07-14; re-ranked 2026-07-15 against 22h of
evidence; **re-ranked again 2026-07-16 after v0.3.0, which shipped the old top two
(admission control and the vital sign) and so collapsed the list upward. A new item
enters at the top — bounding a single session's growth — because it is the one thing
the two July incidents named and nothing yet touches.**

**Why the ranking axis changed.** The earlier lists ranked by "how much of the halts
each removes." But 21:31 showed rtux can remove the halt — spine at 0 faults/s, ladder
fired, everything thawed in 24s — and the machine can still feel terrible, because the
apps it froze were the user's seven working sessions. So the axis is no longer "does
rtux react well" (it does) but "does the machine feel powerful *while* rtux reacts."

**What the first full day of the ceiling said, and still says.** Measured 22h in, idle:

    spine (gnome-shell, IBus)      0 major faults/min   <-- the guarantee, holding
    app.slice                  1,491 major faults/min   <-- the design, working
    memory PSI some avg10           0.00%
    halts since the ceiling             none

The ceiling was the fix for the *spine* guarantee. The spine is resident and the apps
are swapped, which is the architecture doing exactly what it says. The half it does
not touch is the one below.

The catch, and the reason the instrument came before any of this: *cumulative*
`pgmajfault` on gnome-shell reads **182,714**, a scar from the 2026-07-14 incident,
not a wound. The counter is monotonic; it never goes down however healthy the machine
gets. Reading that total as harm — which is exactly what happened while collecting
this data, before the rate was measured — is the same error as the display-string
gate: a confident instrument reporting something that stopped being true. **The metric
is d/dt, and a total is never a health signal.**

1. **Bound what one session can grow into — the gap admission control does *not*
   close.** This is the newly-named top item, and the two incidents name it directly:

       11:57  Froze claude · lexicon (10.2GB)   <- ONE session, 90% of an 11.4GB ceiling
       21:31  7 Claude sessions are using 10.8GB — froze Firefox + 4 sessions in 30s

   The morning event was a *single* session at 10.2GB. `pressured admit` (shipped,
   below) gates the doorway — it can refuse launch #8 — but nothing bounds what an
   already-admitted session then consumes, and a gate at the door does nothing about
   a session that walks in small and grows. This is the harder, truer problem, and it
   is what makes "apps expendable" actually hurt: the expendable app is your work.
   A per-workload cap (`memory.high` on individual app scopes, sized against the
   ceiling) is the shape of the answer, but the hard part is *which* scopes and *how
   much* without turning the ceiling's honest denominator back into a vibe. Design,
   not a knob. Unlisted before 2026-07-16 because the evidence to name it only just
   arrived.

2. **Prove focus-following actually spares the window you're in — measure before you
   build.** Half-built already: `spared_now` = foreground OR recently-typed-in, so the
   mitigator is *supposed* to skip the session you're looking at. The fault-rate meter
   now makes the claim testable, which it never was before. The open question is
   empirical: at 21:31, did the ladder spare the foreground session or freeze it with
   the other four? If it spared it, the felt gap is narrower than it seemed and this
   item is nearly done; if it froze it, `spared_now` has a hole and this is the fix.
   Either way the next move is a *measurement*, not more code — every previous lever
   here was argued into existence; this one can finally be checked.

3. **Wire `admit` to something — it is built and reaches no one.** `pressured admit`
   shipped in v0.3.0 and is aliased in exactly zero places: not `install.sh`, not the
   README, nowhere. A gate no shell calls guards nothing. This is the cheapest move on
   the board — a documented alias, or an installer prompt — and it is the only shipped
   lever aimed at the felt gap at all. It ranks below #1 and #2 despite being nearly
   free because of its own honest limit: it gates launch #8 and does nothing for the
   seven sessions already open, nor for the one that grows after admission (#1). Use:
   `alias claude='pressured admit --want 1024 -- claude'`.

   *Design of the gate, retained because the reasoning is the reusable part.* It gates
   a **launch**, not a prompt or a fan-out: a session costs ~1GB only while active, so
   gating fan-out gates a non-cost (subagents are contexts in one existing process,
   measured twice), gating a prompt destroys work mid-thought, and refusing a launch
   costs nothing — you close something and start again. And it **fails OPEN**: no
   daemon, a stale daemon, an unparseable reply all admit, because "I could not ask"
   and "the answer is no" are different claims and a gate that conflates them refuses
   all work whenever it is itself broken. `tight` admits with a warning — a guard rail,
   not a nanny; one that balks at the first hint of scarcity gets aliased away within a
   day, at which point it guards nothing.

4. **Close the proof loop — a wait, not a build.** The vital sign shipped (`health.rs`):
   the spine's major-fault rate is sampled every tick, surfaced in the HUD as three
   honest states (resident / waiting-on-disk / **unknown**), and written to the journal
   as `SPINE HURT:` when a tick crosses the threshold. The threshold is **100
   faults/tick — derived from measurement, not guessed.** The first version was 20 and
   cried wolf within six minutes of shipping (an idle-desktop spike at PSI 0.1 with
   nobody waiting); it was re-derived from two independent timed events — a 40s
   wake-from-lock at 1.03 ms/fault and a cold embed at 1.99 ms/fault — to catch a real
   stall while ignoring an idle blip. There is nothing left to build here: the
   instrument is armed and has never fired in anger. The standing plan is *learn from
   the next halt* — until one happens and `SPINE HURT` records it, this item is a
   watch, not a task.

5. **`system.slice` is outside the pen — parked.** The partition is spine-vs-`app.slice`,
   leaving a third territory neither pinned nor capped. It ranked #4 in July on the
   strength of `ollama.service`: `memory.high = max`, 59 major faults/min while idle,
   steady disk I/O for a model nobody was using. That concrete harm is going away — the
   decision (2026-07-15) is to drop local ollama. `docker.service` and
   `containerd.service` remain in the same position, but their idle harm is small.
   Parked until something measurable comes back. If it does, the fix is **not** a blunt
   cap on `system.slice` — it holds spine members too (the system bus) — but the same
   honest membership rule applied to it, which is real design, not a knob.

---

*Retained reasoning from the 2026-07-14 ranking, since the arithmetic is the
reusable part. It records why the caller stayed unbuilt for two days and what
finally changed — the conclusion is now superseded by v0.3.0 (see the paragraph
ending this section):*

**Admission control — the primitive, and why it went so long uncalled.** Everything
else rtux does is post-hoc: pressure arrives, we react. The prior art is unanimous that the
guarantee never comes from clever scheduling — it comes from *refusing work that
doesn't fit*. (`advise_claude_sessions` is **not** this: it notifies after the
sessions are already running, and can be ignored.)

The primitive exists: **`ctl budget [MB]`** answers "can this machine afford N
more right now?" and exits 0/1/2. It can only exist because the `app.slice`
ceiling is a standing partition — headroom under it is a real number rather than
a vibe. Without a ceiling there is no denominator and the only honest answer is
"try it and find out", which is the reactive posture the ceiling replaced.

**Nothing calls it, on purpose.** The obvious caller — a PreToolUse gate on
agent fan-out — was measured before building and turns out to gate a non-cost:
subagents are extra contexts inside one existing process, not new processes, so
a fan-out adds no cgroup and no GB. Estimate high and such a gate denies
everything; estimate honestly and it never fires. Either way it is theatre.

The measured arithmetic is different, and it moved the target. A Claude session
costs ~1GB **only while active**; idle ones sit swapped and cost nearly nothing
(observed: 7 sessions, 4.2GB resident against an 11.4GB ceiling, PSI 0.0). So the
halt is a *concurrency* problem — how many sessions are busy at once — not a
count problem, and count-based admission control doesn't touch it.

Stopped here for evidence rather than guessing a third time (2026-07-14). **The
call finally came (v0.3.0, 2026-07-16), but not from this reasoning.** The
2026-07-15 read was "22h, no recurrence, spine at 0 faults/min — the ceiling was
the fix, wiring a caller repeats the day's lesson of building on an unmeasured
premise." Then the halt recurred *the same day* and named a premise this arithmetic
had missed: not a count problem and not a concurrency problem, but *unbounded per-
session growth* — one session at 10.2GB. `pressured admit` shipped as the doorway
gate; the growth bound it does not cover became the new #1 above. The lesson holds
in an inverted form: the caller was right to wait for evidence, and wrong about what
the evidence would say.

**Nothing protects the terminal the user is typing into.** The rule is: mouse,
typing, sound, WM, drawing — protected hard. But *typing* includes the window
the typing lands in, and that window sits in `app.slice`, under the ceiling,
expendable (measured: 30,900 major faults in the user's terminal). The spine
protects the input *method* and then hands the keystroke to an unprotected app.
Focus-following (attention-following, above) is the missing half of "protect
typing" — the focused window is part of the interactive path *while focused*.

## Status

Measured against the north star — *the machine should always feel as powerful as it
actually is* — the two halves stand very differently as of v0.3.0 (2026-07-16).

**The half that is done and provable: the spine never waits on disk.** Measured, not
asserted — a ~130:1 major-fault partition between `app.slice` (33.7M faults since
boot) and the spine (260k), 0 major faults/s at idle, and a `SPINE HURT` black box
armed to record the first time that stops being true. Every claim rtux makes on this
front now has an "I don't know" state rather than a confident wrong answer.

**The half that is barely started: the machine feeling powerful *while* rtux reacts.**
The 21:31 incident on 2026-07-15 is the honest scoreboard entry — rtux did everything
right (spine held, ladder fired, thawed in 24s) and the machine still felt terrible,
because the apps it froze were the user's seven working sessions. "Spine pinned, apps
expendable" stops being true when the apps are your work. `pressured admit` is the
first lever aimed here and it is unwired and partial; the growth bound that would
actually close the gap (#1 above) is unbuilt.

**Validated in the wild, chronologically:**

- **2026-07-12** — the full auto-mitigation fired under real memory pressure: froze a
  1.1 GB browser to hold the desktop responsive and said so legibly, unprompted. The
  user's reaction ("oh nice") is the thesis confirmed — a resource crunch felt like
  the machine's competence, not its failure. Component pieces (freeze/thaw,
  protection, OOM immunity, throttle, socket, HUD, pin) were individually verified
  earlier; the PSI-Critical→freeze trip proven in production.
- **2026-07-14** — a global OOM took down dbus and logged the session out. Drove
  v0.2.1: escalate to killing background hogs before the kernel does, protect the
  session bus, bias the kernel OOM killer away from the spine.
- **2026-07-15** — the ceiling held for 22h with no recurrence (spine 0 faults/min),
  then two halts in one day exposed the *felt* gap above and named its cause
  (unbounded per-session growth). Drove the v0.3.0 measurement work.
- **v0.3.0 (2026-07-16)** — compositor floor tracks its real working set (was a
  guessed 3% of RAM; cost 40s to wake from lock), the spine has a measured vital
  sign, the HUD computes from the mitigator's own predicate instead of a lookalike,
  notifications restored (AppArmor had silently denied them for days), and `pressured
  admit` ships as the first admission-control caller.
