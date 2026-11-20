# Post-mortem: desktop lockup, 2026-11-20 ~11:28–11:44 PST

The desktop became unresponsive for about 15 minutes and was power-cycled
(logind `Power key pressed short` at 11:44:39 and 11:44:42). rtux was running the
whole time, never killed anything, and was standing down — nothing frozen — for
the final two minutes while the compositor was faulting 600–850 times a second.

## Sources

- `journalctl -b -1 -u rtux` (pid 7165), 10:00–11:44
- `sar -r -S -B -W -q ALL -d -f /var/log/sysstat/sa20`, 10-minute samples
- Current swap and compositor cgroup config (`swapon`, `memory.min`, `memory.swap.max`)

rtux's own PSI trend and event log are in-memory only and died with the boot.
The final four minutes fall between sar samples, so the 11:40 sample is the
last hard number on memory and IO; everything after it comes from the rtux journal.

## What the machine looked like

| sar sample | swap used | memory PSI some/full avg10 | io PSI some/full avg10 | dm-0 queue / await |
|---|---|---|---|---|
| 11:20 | 8.5 GB (33%) | 43.7 / 29.0 | 36.1 / 9.2 | 716 / 62 ms |
| 11:30 | 9.3 GB (36%) | 0.0 / 0.0 | 4.8 / 1.3 | 758 / 55 ms |
| 11:40 | 12.2 GB (48%) | 17.2 / 15.5 | 60.6 / 49.5 | 860 / 101 ms |

Swap is three devices with 24.4 GB in total: zram 7.4 GB (priority 100), `/swapfile` 16 GB on the
LUKS+LVM root, and a 980 MB LVM swap partition. `vm.swappiness=180`.
From 10:40 on, swap usage exceeded zram's capacity, and zram0 writes stayed at 0–2 kB/s in
every sample. **Every page swapped out in this window went to the encrypted
NVMe swapfile**, not to compressed RAM.

`SPINE HURT` fired 954 times between 10:00 and 11:44, at 33–60 lines a minute from 11:28,
naming the compositor each time.

## Timeline

- **11:20–11:42**: rtux freezes and thaws the same scopes repeatedly. One Claude session was
  frozen at 11:22, 11:30, 11:31, 11:32 and 11:34, and thawed 10–40 s after each.
  Across this window, 13 forced reclaims paged out **9.4 GB**, and every thaw faulted it
  back in from disk.
- **11:28**: the spine-fault rate jumps to 33–57 lines a minute and stays there.
- **11:39–11:42**: freezes of a batch job, a local model server and another Claude session, each
  followed by a page-out and then a thaw ~30–40 s later.
- **11:42:31**: `pressure cleared — resumed 2 paused app(s)`. After this rtux freezes nothing.
- **11:42–11:44**: the compositor takes 200–850 major faults every second. The only rtux
  output is the quiescence observer and two `4 Claude sessions using 7.7GB` advisories.
  Advisories print only on Elevated ticks, and the second one could only print after
  a `recover()`, which needs 10 consecutive Normal ticks. So memory PSI was moving between
  Elevated and Normal while the UI was frozen. It never read Critical long enough to freeze anything.
- **11:44:39**: power button.

## Root causes

### 1. The kill gate measures swap capacity, but the failure is swap bandwidth

`kill_worst` (`src/mitigate.rs:465`) returns unless
`swap_used_fraction() >= SWAP_HIGH_WATER` (0.85), and that fraction is computed over
`SwapTotal`, which includes the 16 GB disk swapfile. The desktop fails once
**zram is full** and anon paging moves to disk. That happens at about 30% of total
swap. 85% means about 20.7 GB used, which is roughly 13 GB already written through
LUKS to the NVMe. At the 11:30→11:40 growth rate (+3 GB per 10 min), the gate would have
opened around 12:07, more than 20 minutes after the desktop died. On this
swap layout the kill rung cannot fire in time.

### 2. The ladder looks only at memory PSI avg10; the signals that saw the outage drive nothing

`Daemon::reconcile` (`src/main.rs:221`) classifies the situation from
`/proc/pressure/memory` and nothing else. The spine fault meter (`health.rs`) is
observation-only by design, and `/proc/pressure/io` is read only by `ctl status`
(`src/cli.rs:13`). Here the stall was IO: at 11:40, io full avg10 was 49.5% against
memory full 15.5%. In the final two minutes, memory PSI fell far enough that
rtux thawed everything and stood down. The compositor's faults are not anon swap-ins. It
has `memory.swap.max=0` and `memory.min=512M`, so these are file-page refaults
(libraries, shaders, mapped files), and they sit in an 860-deep dm-0 queue at ~100 ms
each. That stall shows up in IO PSI and the spine meter, and only weakly in memory PSI.

### 3. rtux's own freeze → reclaim → thaw cycle was the main source of disk swap IO

Each freeze calls `memory.reclaim` for the whole scope. The only guard is
`swap_used_fraction() < SWAP_RECLAIM_CEILING` (0.70) (`src/mitigate.rs:369`), which
has the same blind spot as cause 1: it passed at 33–48% while zram was full, so the
"hibernate to compressed RAM" in the code comment was actually a write to the encrypted
swapfile. Then `RECOVER_AFTER_NORMAL_SECS = 10` (`src/main.rs:27`) thaws the
scope after 10 calm seconds. The thawed process faults its working set back in from
disk, pressure returns, and the same scope is picked again. That added 9.4 GB of forced
write-then-read in 22 minutes on a disk that was already 70–90% utilized. In
the last minutes, the compositor's file faults were queued behind that traffic.

## Not causes

- Kernel OOM: never fired, and there was no OOM in `kern.log`. Memory was not exhausted, and
  swap had more than 12 GB free. The machine was IO-starved.
- systemd-oomd: one kill (a user service, 10:40:36), not involved at 11:4x.
- The ctl read-timeout branch and the socket-activation changes: the daemon ran
  continuously as pid 7165 the whole time and logged every second.

## What would have saved it

These are listed in order of how directly they address the failure.

1. **Gate kill and reclaim on zram fullness and disk-swap use.** Read per-device usage
   from `/proc/swaps`. "zram full and disk swap growing" (or disk-swap write
   rate above X) is the precipice, and `memory.reclaim` must be skipped once
   zram is full. That one change stops cause 3 and opens the kill gate in
   time for cause 1.
2. **Let spine harm escalate.** Sustained `SPINE HURT` (say N of the last 10 ticks above
   the threshold) or IO PSI full above a threshold should count as Critical. Keep it
   from triggering thaw recovery while the spine is still faulting.
3. **Stop the ping-pong.** Use a longer, growing thaw hysteresis per scope (a scope frozen
   twice in an episode stays frozen until the spine is quiet for minutes), or
   escalate a repeatedly re-frozen scope straight to the kill rung.

None of this has been implemented or tested; this document is diagnosis only.
