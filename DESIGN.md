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

## Levers not yet pulled

Ranked by how much of "things chug to a halt all the goddamn time" each one
actually removes. Recorded 2026-07-14, after the realignment; **re-ranked
2026-07-15 against 22h of evidence, which moved #3 to the top and demoted #1.**

**What the first full day of the ceiling actually said.** Measured 22h in, at idle:

    spine (gnome-shell, IBus)      0 major faults/min   <-- the guarantee, holding
    app.slice                  1,491 major faults/min   <-- the design, working
    memory PSI some avg10           0.00%
    halts since the ceiling             none

The ceiling was the fix. The spine is resident and the apps are swapped, which is
the architecture doing exactly what it says. **rtux is currently meeting its
promise** — a first, and the reason the ranking below changed.

The catch, and the reason the instrument came first: *cumulative* `pgmajfault` on
gnome-shell reads **182,714**, and that number is a scar from the 2026-07-14
incident, not a wound. The counter is monotonic; it can never go down no matter
how healthy the machine gets. Reading that total as harm — which is exactly what
happened while collecting this data, before the rate was measured — is the same
error as the display-string gate: a confident instrument reporting something that
stopped being true. **The metric is d/dt, and a total is never a health signal.**

1. ~~Admission control~~ **→ demoted. The evidence gate resolved: no.** The primitive
   (`ctl budget`) ships and works, and the plan of record was to wire a caller only
   if the halt recurred with the ceiling in place. It has not recurred in 22h, and
   the spine is at 0 faults/min. Per the gate's own terms, **the ceiling was the fix
   and a gate would be dead code.** Leave `ctl budget` unwired; it costs nothing
   sitting there and is ready if a future incident names something to gate on.
   Details of why the obvious caller was theatre are kept below, because the
   reasoning is the reusable part.

2. **The guarantee is invisible** — *now built (2026-07-15), see `health.rs`.* The
   spine's major-fault rate is sampled every tick, surfaced in the HUD as three
   honest states (resident / waiting-on-disk / **unknown**), and written to the
   journal as `SPINE HURT:` when a tick crosses the threshold. That last part is
   the point: the standing plan is *learn from the next halt*, and until now nothing
   would have recorded it. An evidence gate with no instrument behind it is a wish.

   The threshold (20 faults/tick) is a **guess, marked as one in the source.** The
   spine idles at 0, so the shape is right; where "noticeable" starts is unknown
   because the only incident we have was never instrumented. Replace it with the
   measured number after the first capture — and do not tune it against a healthy
   machine, which is guessing wearing a lab coat.

3. **Nothing protects the terminal the user is typing into** — the next real build.
   Now measurable: with the fault-rate meter live, focus-following can be judged by
   whether the focused window's fault rate actually drops, rather than by whether
   the idea sounds right. Every previous lever was argued; this one can be tested.

4. **`system.slice` is outside the pen.** The partition is spine-vs-`app.slice`, but
   there is a third territory neither pinned nor capped. Measured 2026-07-15:
   `ollama.service` has `memory.high = max`, sits at 280MB resident with 559MB
   swapped, and takes **59 major faults/min while idle** — steady disk I/O for a
   model nobody is using. It is not spine (nothing breaks if it's slow) and not
   `app.slice` (so the ceiling never reaches it); `docker.service` and
   `containerd.service` are in the same position. The harm is small today and the
   hole is not: nothing bounds what a background service loads. **Do not just cap
   `system.slice`** — it holds spine members too (the system bus). It needs the same
   membership rule applied honestly, which is a real piece of design, not a knob.

---

*Retained reasoning from the 2026-07-14 ranking, since the arithmetic is the
reusable part:*

**Admission control — the primitive, and why nothing calls it.** Everything else rtux
does is post-hoc: pressure arrives, we react. The prior art is unanimous that the
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

Stopped here for evidence rather than guessing a third time (2026-07-14). **That
gate has since resolved (2026-07-15): 22h, no recurrence, spine at 0 faults/min
— so the ceiling was the fix and the caller stays unbuilt.** Wiring one anyway
would repeat the day's actual lesson — building on an unmeasured premise — with
more ceremony.

**Nothing protects the terminal the user is typing into.** The rule is: mouse,
typing, sound, WM, drawing — protected hard. But *typing* includes the window
the typing lands in, and that window sits in `app.slice`, under the ceiling,
expendable (measured: 30,900 major faults in the user's terminal). The spine
protects the input *method* and then hands the keystroke to an unprotected app.
Focus-following (attention-following, above) is the missing half of "protect
typing" — the focused window is part of the interactive path *while focused*.

## Status

**Validated in the wild (2026-07-12):** the full auto-mitigation fired under real
memory pressure — froze a 1.1 GB browser to hold the desktop responsive and said
so legibly, unprompted. The user's reaction ("oh nice") is the thesis confirmed:
a resource crunch felt like the machine's competence, not its failure. The
component pieces (freeze/thaw, protection, OOM immunity, throttle, socket, HUD,
pin) were individually verified earlier; the PSI-Critical→freeze trip is
now proven in production.
