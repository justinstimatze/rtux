#!/usr/bin/env python3
"""A stand-in for an interactive foreground app. Holds a working set and, every
tick, sweeps it (touching each page) plus a little compute — like redrawing a
frame. Records how long each sweep takes; long sweeps mean its pages were pushed
to swap and had to fault back, i.e. the jank a user would feel as lag.

Emits a JSON summary to stdout on exit. Pure stdlib, no deps."""
import json
import os
import sys
import time

WS_MB = int(os.environ.get("VICTIM_WS_MB", "250"))
DURATION_S = float(os.environ.get("VICTIM_DURATION_S", "20"))
PAGE = 4096

# Allocate and fault in the working set.
buf = bytearray(WS_MB * 1024 * 1024)
for i in range(0, len(buf), PAGE):
    buf[i] = 1

sweeps = []
end = time.monotonic() + DURATION_S
while time.monotonic() < end:
    t0 = time.perf_counter()
    s = 0
    for i in range(0, len(buf), PAGE):
        s += buf[i]          # touch every page — faults if it was swapped out
        buf[i] = (s & 0xff)  # dirty it so it can't be dropped for free
    dt = (time.perf_counter() - t0) * 1000.0  # ms
    sweeps.append(dt)

sweeps.sort()
n = len(sweeps)


def pct(p):
    if n == 0:
        return 0.0
    return sweeps[min(n - 1, int(p / 100.0 * n))]


print(json.dumps({
    "ws_mb": WS_MB,
    "sweeps": n,
    "p50_ms": round(pct(50), 1),
    "p95_ms": round(pct(95), 1),
    "p99_ms": round(pct(99), 1),
    "max_ms": round(sweeps[-1], 1) if n else 0.0,
    "janky_over_50ms": sum(1 for x in sweeps if x > 50),
    "janky_over_200ms": sum(1 for x in sweeps if x > 200),
}))
