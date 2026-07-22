# Changelog

Notable changes to rtux. Versions follow [semver](https://semver.org); the git
tag is the source of truth (the binary reports it via `pressured --version`).

## [Unreleased]

### Fixed
- **The freeze rung ran open-loop, and paused far more than it needed to.** The
  daemon ticks at 1Hz but steers on `memory.some.avg10` — a *ten-second* average —
  so a freeze could not show up in the signal for ~10s while `escalate()` kept
  firing every tick. It overshot by roughly the ratio of the two cadences:
  six apps frozen in six seconds, a shape that ran 69 times in six hours pausing
  4–7 apps each. That was the whole of "rtux pauses everything". `FREEZE_SETTLE`
  (10s, one PSI window) closes the loop — act, wait for the sensor to reflect it,
  decide again. An episode's *first* freeze is never delayed and `kill_worst` is
  deliberately not gated, so a genuine runaway is still caught at the old speed.
  Confirmed on the live daemon: the same burst now spaces 10–11s apart.
- **Focus thawed the wrong cgroup, so alt-tabbing to a paused window did nothing.**
  `protect_foreground` thaws the focused *scope*, which is the whole story for a
  browser and wrong for a terminal — the shell or agent runs in a **sibling** scope
  systemd created, so focus thawed something that was never frozen while the paused
  session stayed paused. Twelve hours of journal showed 6 focus-thaws against ~69
  freeze cycles, not one of them a spawn scope. `guard::thaw_foreground_related`
  now revives what focus actually owns, reusing `classify::is_foreground_related` —
  the same predicate the eviction path uses to *spare* the foreground terminal — so
  what focus protects and what focus revives cannot drift apart. Under tmux,
  ancestry cannot work at all (a pane descends from the tmux server, not the
  terminal), so `tmux-spawn` scopes fall back to a deliberately coarser question:
  is the focused window a terminal running tmux? Confined to the thaw path on
  purpose — widening the *freeze* path the same way would leave the rung no victims
  under real pressure. The call also had to move ahead of `do_foreground`'s
  `"unchanged"` early-return, since one terminal process owns every terminal window
  and refocusing was therefore "unchanged" every time.

### Changed
- **Toasts are reserved for kills.** On a machine that lives at the memory limit,
  the freeze notice fired on every freeze — a nine-app episode was nine popups —
  burying the one notice that matters. Freeze, pressure-rising, recovery, and the
  too-many-sessions advisory no longer toast at all; they live in the journal, in
  `ctl history`, and in the tray/HUD's ambient state (none of which Do-Not-Disturb
  suppresses — closes gh #1 by construction). The one surviving toast is a **kill**:
  destructive, irreversible, `critical` urgency so it shows under DND. With the
  freeze notice gone, its whole actionable-notification path — `notify_action` plus
  the dbus-monitor `ActionInvoked` parser — was dead and is removed (net −289 lines).
- **The eviction rung freezes the *idlest* big consumer, not merely the largest.**
  It ranked by size alone, so on a box running many Claude sessions it grabbed the
  user's working set — pausing a session mid-response is the worst felt outcome.
  `escalate()` now orders eligible scopes by a recent `cpu.stat` activity delta
  (`pick_freeze_index`) and pauses the idlest, size as the tiebreak. An ordering,
  not an Idle threshold (that calibrated cutoff is still the measure-first
  follow-up), so it's safe ahead of the measurement and degrades to largest-first on
  an episode's first tick.

### Added
- **Focus thaws.** Focusing a window rtux had frozen now unfreezes it immediately,
  rather than waiting for pressure to clear and the thaw hysteresis to elapse. Focus
  is intent; an unresponsive focused window is the exact jank rtux exists to prevent.
  (As first shipped this only reached the focused *scope*; see Fixed above for the
  sibling-scope and tmux cases, which are most of the real workload.)
- **A `focus thawed nothing:` diagnostic**, printed only when focus arrived, something
  was frozen, and nothing was revived — naming the foreground pid, whether a tmux
  client was reachable from it, and what was frozen. Without it, "the focus event
  never arrived" and "the predicate declined" are the same silence in the journal,
  which is what turned one measurement into three rounds of guessing.
- **The quiescence instrument measures the whole distribution.** It had logged only
  the idle tail (scopes under the 2% gate), which can never reveal the idle/active
  valley the Idle threshold lives in. `ActivityMeter::observe` now also emits a
  histogram over all sizeable scopes (every ~5 min), and candidate labels resolve to
  the rich `claude · dir` instead of a generic `Terminal (child)`.

## [0.3.0] — measured, not guessed

Every number rtux acted on used to be a guess, and on 2026-07-15 the guesses were
billed. The compositor's floor was `total_ram/33` — 460MB against a measured 1.32GB
working set — and waking the machine from lock took 40 seconds while the compositor
faulted its own windows back off the swapfile. The spine's health was unmeasured, so
nothing could tell you whether the one guarantee rtux makes was holding. The HUD
tagged sessions `critical` while the daemon froze those same sessions. Notifications
had been dead since the daemon was hardened, silently, for days.

This release replaces each of those with a measurement: the floor now tracks
`memory.current + memory.swap.current` (invariant to eviction, which is exactly why
it is the right quantity), the spine reports a fault **rate** rather than a
cumulative total (a total is a scar, a rate is a wound), the HUD computes its claims
from the mitigator's own predicate, and every user-facing claim gained an "I don't
know" state. `pressured admit` ships as the first lever aimed at the gap the day's
two incidents exposed — rtux can react perfectly and the machine can still feel
terrible, because "spine pinned, apps expendable" stops being true when the apps
are your seven working sessions.

### Added — `pressured admit`: the admission-control caller

`ctl budget` has been sitting unwired since it shipped, deliberately, waiting for an
incident to name what to gate. Two arrived on 2026-07-15:

    11:57  Froze claude · lexicon (10.2GB)   <- ONE session, 90% of an 11.4GB ceiling
    21:31  7 Claude sessions are using 10.8GB — froze Firefox + 4 sessions in 30s

The second is the instructive one: rtux did everything right — the spine held at 0
major faults/s, the ladder fired, everything thawed 24s later — and the machine
still felt terrible, because every app the user was looking at was paused. "Spine
pinned, apps expendable" holds right up until the apps are your seven working
sessions. Reacting well is not the same as not needing to react.

    alias claude='pressured admit --want 1024 -- claude'

**Gates a launch, not a prompt or a fan-out.** A session costs ~1GB only while
active. Gating fan-out gates a non-cost (subagents are contexts in one existing
process, not new processes — measured twice). Gating a prompt destroys work in
flight. Refusing a *launch* costs nothing: you close something and start again.

**Fails OPEN.** No daemon, stale daemon, unparseable reply → the command runs.
"I could not ask" is not "the answer is no", and a gate that conflates them refuses
all work whenever it is itself broken. `tight` admits too, with a warning — a guard
rail, not a nanny. `--force` always wins. `exec`s rather than spawns, so the alias is
invisible when it admits (exit codes, signals and the terminal all pass through).

A refusal names what to close. The first draft filtered on the list reply's
`freezable` flag and named the wrong things — Firefox (1.3GB) and Ollama (274MB)
while 7.4GB of Claude sessions went unmentioned. See the known issue below.

### Fixed — the HUD said your sessions were protected while the daemon froze them

Two notions wearing one name. The list reply computed
`freezable = has_freeze && !never_freeze(..)` — but `never_freeze` answers *"may a
CLIENT freeze this via ctl?"* and deliberately refuses every terminal (its own doc:
"the conservative default for user-initiated actions"). The auto-mitigator asks a
different question, answered in `denied()`, which checks only `hard_exempt` plus the
dynamic foreground/live spares.

So every Claude session in a `tmux-spawn` scope reported `freezable: false` and the
HUD tagged it `critical` — reading as "rtux will never touch this" — while the
daemon froze those same sessions under pressure. Measured 2026-07-15: `Froze
claude · rtux (1.6GB)` against a list reply calling that exact scope unfreezable.
The `◀ hog` marker had the same defect from the same cause: it flagged Firefox
(1.2GB) as the next thing to be paused while `claude · lexicon` (2.0GB) sat labelled
`critical` — and lexicon is what actually got frozen. Its comment even stated the
intent the code violated ("so the top-consumer marker never promises a pause that
won't come").

The daemon's behaviour is correct per DESIGN (apps within terminals are expendable,
and the terminal you're actually *in* is spared dynamically). The display was the
wrong half. Both fields now use the mitigator's own predicate.

**`spared` is now a separate fact from `freezable`,** because they are different
promises and collapsing them is what let the bug read as "protected forever":

- `critical` — rtux will *never* pause this (structural: the spine, the system).
- `spared` — rtux may pause this, but not while you're using it (momentary:
  foreground, or a recent keystroke).
- `live` / `◀ hog` — rtux may pause this, and the hog is next.

Also pinned by test: **never pass a display label to `hard_exempt`/`never_freeze`.**
They match by substring against a list containing the protector's own names, so
"claude · rtux" — the label for a session working on this repo — contains "rtux" and
hard-exempts itself; a directory named `dbus`, `systemd` or `pressured` would do the
same. Not a live bug (both callers pass `cgroup_to_app_name`'s output), but an
earlier draft of the test tripped it, and it fails silently.

### Fixed — notifications, silently denied by AppArmor since the daemon was hardened

rtux froze five apps during the 2026-07-15 incident and told the user about none of
them. Only the journal knew. Silent intervention is the legibility thesis inverted:
the machine reaching into your session without saying so is exactly the "opaque
automation" failure DESIGN.md defines rtux *against*.

The kernel's own account:

    apparmor="DENIED" operation="connect" info="Failed name lookup - disconnected path"
    error=-13 profile="notify-send" name="run/user/1000/bus" fsuid=1000 ouid=1000

Read `name="run/user/1000/bus"` — **no leading slash.** Ubuntu ships an AppArmor
profile for notify-send permitting `/run/user/*/bus`. `ProtectSystem=full` makes
systemd build the daemon a mount namespace; inside one, AppArmor cannot resolve that
path back to the root namespace, calls it "disconnected", and hands the matcher a
*relative* name the profile's absolute rule cannot match. Denied → EACCES →
"Could not connect: Permission denied", which reads exactly like a file-permission
problem and is nothing of the kind.

Everything you would suspect first is innocent, and each was measured to be: the
socket is `srw-rw-rw-`; `setuid(1000)` succeeds; `/run/user/1000` (dev 0:113,
mode=700 uid=1000) IS in the daemon's namespace per `/proc/PID/mountinfo`;
`MemoryMax` and seccomp are uninvolved. `ProtectSystem` looked guilty only because
the first bisect dropped directives from a set where it was the **only** one that
creates a mount namespace — `PrivateTmp` and `ProtectHome=read-only` fail
identically. The namespace is the cause; ProtectSystem was a proxy for it.

**Fixed by speaking D-Bus directly through `gdbus`, at zero cost to hardening.**
AppArmor attaches profiles on `exec`; the daemon is `unconfined`, and of the
binaries that reach the bus only notify-send carries a profile — gdbus, dbus-send
and busctl do not. Verified under the unit's *full* confinement: notify-send FAILS,
gdbus WORKS, zero denials. The alternative was deleting `ProtectSystem=full` from a
root daemon that writes cgroups and other processes' `oom_score_adj` — trading real
hardening to avoid understanding a bug.

`notify-send --wait` did the action-button waiting for us; D-Bus makes that manual,
so the click now arrives as an `ActionInvoked` signal watched via `dbus-monitor`.
The monitor starts **before** the notification is sent: the signal is broadcast
once, and a click landing between send and watch would be lost, leaving a button
that does nothing — worse than no button, since the machine looks broken rather than
quiet. Parser tested against verbatim bus output from a real click, including the
bus's own NameAcquired/NameLost lines (bare `string` bodies with no uint32) that
turned up in the capture and were not predicted.

New runtime deps: `libglib2.0-bin` (gdbus), `dbus-bin` (dbus-monitor). `install.sh`
warns rather than fails if absent — the journal record survives either way.

### Fixed — the compositor's floor was set to a third of the compositor

`compositor_memory_min` was `total_ram / 33` — 3% of RAM, capped at 1GB, a bare
percentage whose doc comment called it "hardware heuristics". Measured 2026-07-15,
minutes after a wake-from-lock took ~40 seconds to show a login field:

    memory.min      460MB   <-- what the 3% promised
    memory.current  781MB
    memory.swap     538MB   <-- evicted anyway

**`memory.min` was never violated — it was obeyed exactly.** It guaranteed 460MB,
the kernel honoured 460MB, and reclaimed the 538MB above the line, because
everything above the line is by definition fair game. The wake then faulted 38,836
pages back off the swapfile one at a time. The most load-bearing number in the
daemon — the one deciding whether the desktop stays resident — was a guess aimed at
a third of its target. A guarantee aimed below its target isn't weak, it's decor.

The floor is now **measured**: `memory.current + memory.swap.current`, which is
invariant to how much has already been evicted (a squeezed compositor and a
resident one size identically). Reading `current` alone would ratchet the floor
down as pages left, chasing the eviction it exists to prevent. Capped at half the
desktop reserve so a ballooning compositor can't pin the machine, and `desktop_reserve`
is now factored out so the cap and the app ceiling cannot drift apart. On this box
the floor goes 460MB → ~1.29GB. `memory.min` is a protection, not an allocation, so
the unused portion costs nothing.

### Added — the spine finally has a vital sign

- **`health.rs`: the spine's major-fault rate, sampled every tick.** rtux's whole
  claim is "the interactive path stays resident", and until now nothing measured
  whether that was true. The HUD gains one line reporting it, and the daemon writes
  a `SPINE HURT:` line to the journal when a tick crosses the threshold — so the
  next incident leaves a record even if it happens at 3am. The standing plan is to
  learn from the next halt; an evidence gate with no instrument behind it is a wish.

  **The rate, never the total.** `pgmajfault` is monotonic — it never decreases —
  so a cumulative reading is a scar, not a wound. Measured 22h after the incident:
  gnome-shell's total read 182,714 while its actual rate was 0/min. A HUD wired to
  the total would show a permanently-red number that can never improve.

  Reports three states, not two: resident / waiting-on-disk / **unknown**. An empty
  spine sums to zero faults, and zero faults would otherwise render as a confident
  green "clean" — a blind meter posing as a healthy one, which is the same defect
  as the display-string gate and `render_budget`'s missing-verdict default.

  **The threshold is derived, after the first one cried wolf.** It shipped at 20
  faults/tick, reasoned from "the spine idles at 0, so any sustained nonzero is
  abnormal", and fired within six minutes of install on an idle desktop at PSI 0.1
  with nobody waiting. The error was reasoning about fault *counts* when the thing
  that matters is *how long the user waits*. Two independent events on this machine
  give the missing constant:

      wake-from-lock     38,836 faults /  40.0s = 1.03 ms/fault
      cold ollama embed  56,625 faults / 112.8s = 1.99 ms/fault

  They agree within ~2x across different subsystems and page sets, which is what
  licenses using them. At 1–2 ms/fault, **100 faults/tick ≈ 100–200ms of the
  interactive path on disk** — roughly the floor of perception. The 40s wake ran at
  ~950/s and fires loudly; the idle 20/s correctly says nothing. (Both figures are
  10–20x this box's NVMe spec and that gap is unexplained — the *agreement* is the
  load-bearing part, not the absolute value. Do not present ms/fault as understood.)

### Fixed — the journal is a black box, so it has to stay readable

Measured six minutes after a restart: 138 lines, of which 36 were three fault-in
messages repeating every 30s and 12 were the bulk ceiling re-announcing an unchanged
number. A real `SPINE HURT:` line was in there, drowning.

The rule now encoded in `once_per`: **an event is logged every time; a standing
condition is logged once.** "Faulted in 64MB" is an event. "This cgroup's swap is
shmem and unreachable" was already true last pass and will be true next pass —
re-stating it every 30s is not reporting, it is noise that buries reporting. Same
reasoning `announced` already applied to protection. Failures stay loud on every
pass: unlike a success, a failure is not a settled fact.

### Note — the app.slice ceiling is validated

22h of evidence: spine at 0 major faults/min, app.slice at 1,491/min (the design
working as intended), memory PSI 0.00, and no recurrence of the halt. The ceiling
was the fix. Per the evidence gate set when `ctl budget` shipped, admission control
stays deliberately unwired.

### Fixed — the 2026-07-14 session logout

A second global-OOM logout, with rtux running and believing it was working. Four
independent defects, each of which alone was enough to lose the session.

- **rtux ranked by the one metric that lies under pressure.** `collect_freezable`
  gated and sorted on `memory.current`, which counts only *resident* pages — so it
  collapses as a cgroup swaps out, under exactly the condition rtux exists for.
  Real hogs paged out below the 512MB floor (one showed 857MB resident against
  1.5GB swapped: a 2.4GB process ranked as 857MB), the largest-first loop `break`s
  at the floor, and both the freeze and kill rungs found *nothing to do*. rtux sat
  at critical for three minutes having frozen twice, and never fired the kill rung
  at all — while the kernel OOM proved swap was exhausted, far past the 85% gate.
  Now ranks on **footprint = `memory.current` + `memory.swap.current`**: a swapped
  page is not a freed page.
- **rtux's own reclaim fired the OOM.** It forced a 4.5GB `memory.reclaim` into an
  already-full swap (zram 6.5/7.4GB, swapfile 8.8/16GB) and the kernel's global
  OOM killer fired *in the same second*. Reclaim is now gated on swap headroom
  (`SWAP_RECLAIM_CEILING`) — freezing alone already stops the growth, which is the
  part that matters.
- **The OOM ranking was inverted, so the kernel could only kill the session.** The
  fattest consumers self-protect at `oom_score_adj=-1000` (~7GB of Claude
  sessions, structurally immune), while the session's own services sat at +200 and
  `systemd --user` at +100. The kernel dutifully killed the user manager — which
  *is* the logout. rtux now biases background hogs to +500 during pressure, giving
  the killer a resumable victim instead of the desktop. Being un-killable is not
  enough; someone must be *more* killable.
- **Spine protection decayed and never came back.** The daemon re-tried protection
  only `if !protected`, latching true after the first success. But `oom_score_adj`
  is per-*process*: a service restart or re-login brings up new pids that never get
  it. Measured after the logout — session dbus and pipewire sitting at +200 while
  the daemon believed the spine was protected. Now re-asserted every 30s,
  unconditionally.

### Fixed — desktop responsiveness
- **The compositor was half-swapped to disk.** `memory.min` only stops reclaim
  *below* the floor; everything above it stayed fair game, and `memory.swap.max`
  was wide open. Measured: 523MB resident vs **530MB in swap**, faulted back off
  the on-disk swapfile (zram having long since filled) on every window switch —
  i.e. "I can barely switch windows and the keyboard lags". Protected cgroups under
  1/8 of RAM are now pinned out of swap entirely (`memory.swap.max=0`); larger ones
  keep their swap door open so favouring the foreground can't turn it into a black
  hole.
- **Freeze notices no longer claim "moved to compressed RAM".** zram is only the
  first swap device; once full, everything overflows to the disk swapfile. The
  cheerful notice was describing gigabytes of disk writes that were stalling the
  desktop it claimed to protect. Now says "paged out".

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

[Unreleased]: https://github.com/justinstimatze/rtux/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/justinstimatze/rtux/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/justinstimatze/rtux/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/justinstimatze/rtux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/justinstimatze/rtux/releases/tag/v0.1.0
