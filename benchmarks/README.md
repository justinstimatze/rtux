# rtux benchmarks — does the intervention actually help?

A small, **contained** experiment to answer the honest question: when rtux freezes
a memory hog, does a foreground app actually stay more responsive, or is it just
a reassuring notification?

## What it measures

- **victim.py** — a stand-in foreground app. It holds a working set and, each
  tick, sweeps every page (plus a little compute), like redrawing a frame. It
  records how long each sweep takes. Long sweeps = pages that were pushed to swap
  and had to fault back = the lag a user feels.
- **hog.py** — a runaway app: grows its memory, then keeps its whole set *hot*
  (so it genuinely competes with the foreground for RAM — this is what causes real
  thrash, not just a one-time allocation).
- **bench.sh** — runs the victim against the hog twice: **A) unmanaged** (hog runs
  free) and **B) freeze** (partway through, we write `cgroup.freeze` on the hog —
  the exact primitive `actions::freeze_cgroup` uses), then compares.

## Safety

The whole thing runs inside a memory-capped `bench.slice`, so the pressure is
**contained** — your real apps are never touched, and the machine as a whole is
never driven into swap. Worst case, the benchmark's own processes are OOM-killed
inside their sandbox.

```sh
./benchmarks/bench.sh
# tune: VICTIM_WS_MB, HOG_MAX_MB, FREEZE_AT_S, and the MemoryMax in bench.sh
```

## What we found (and the honest reading)

A representative run (400 MB victim, RAM capped to 700 MB, hog to 900 MB, freeze
at 7 s of a 26 s run), on a machine **with zram enabled**:

| metric | A: unmanaged | B: freeze | |
|---|---|---|---|
| worst sweep | 105 ms | 57 ms | ~1.9× better |
| janky sweeps (>50 ms) | 8 | 2 | ~4× fewer |
| p95 sweep | 39 ms | 33 ms | ~1.2× better |
| completed sweeps | 914 | 983 | more = less stalled |

**Freezing the hog is a real, measured improvement — not cosplay** — roughly 4×
fewer janky frames and half the worst-case stall.

But note the *absolute* numbers: even the unmanaged case never stalls
catastrophically. That's the second finding, and it's the honest one: **zram is
doing most of the work.** The hog's cold pages compress into zram fast enough that
the foreground's hot working set mostly survives, so the freeze is the *second*
line of defense, not the first. This matches rtux's own thesis — zram raises the
wall; the freeze/throttle ladder governs the wall you occasionally still hit.

**Caveat / what this does *not* capture:** the original catastrophe that motivated
rtux — a global out-of-memory storm with the *compositor itself* starved and
seconds of cursor lag — happened without zram, spilling to slow disk swap. This
contained, zram-backed benchmark deliberately can't reproduce that severity
safely. So read these numbers as a *lower bound* on the benefit: the worse the
real thrash (no zram, disk swap, the compositor evicted), the more the
freeze-and-protect intervention matters.
