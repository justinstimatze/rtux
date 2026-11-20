# Plan: keep GNOME from stalling (after the 2026-11-20 lockup)

Goal: the compositor never waits on disk, so the user can always close things
themselves. No key chords, and the auto-kill gate does not move earlier.

## Host (user runs with sudo)
- `scripts/host-zram-to-zswap.sh`: zswap (zstd, 25% pool, shrinker) over
  /swapfile, zram removed, swappiness 180 -> 100. `--undo` reverts.

## Daemon (branch swap-gate-per-device)
Steps 1, 2 and 4 are built. Step 3 is built for sustained spine faults only;
the file-refault and IO-PSI triggers are not. Steps 5 and 6 are open.
1. Thaw orphaned freezes at startup and log them.
2. Reclaim: skip forced memory.reclaim when IO PSI full is high or anon would go
   to disk (per-device /proc/swaps; under zswap, set memory.zswap.writeback=0
   on the frozen scope instead). Cap each reclaim.
3. Spine harm escalates: sustained compositor major faults, or file-refault rate
   (lmkd's workingset_refault_file / file LRU), or IO PSI full counts as Critical,
   and it blocks the thaw.
4. Thaw backoff: per-scope growing hysteresis; stagger thaws one at a time.
5. Measure: per-cgroup workingset_refault_file for the compositor; does its
   memory.min cover the libraries it faults (charging rule)?
6. Kill gate: leave at 85% total, or make it tier-aware but no earlier. Backstop only.

Sources: docs/postmortem-2026-11-20-lockup.md; docs/lit-review-2026-11-20.md.
