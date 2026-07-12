#!/usr/bin/env python3
"""A runaway memory hog: grows its resident set steadily, touching each new
chunk so the pages are real, until killed or frozen. Stands in for the browser
tab / build job that balloons and drives the machine into swap thrash."""
import os
import time

STEP_MB = int(os.environ.get("HOG_STEP_MB", "50"))
MAX_MB = int(os.environ.get("HOG_MAX_MB", "4000"))
PAGE = 4096

chunks = []
grown = 0
while grown < MAX_MB:
    c = bytearray(STEP_MB * 1024 * 1024)
    for i in range(0, len(c), PAGE):
        c[i] = 1
    chunks.append(c)
    grown += STEP_MB
    time.sleep(0.15)  # steady growth, not an instant spike

# Keep the WHOLE set HOT — a real runaway app keeps using its memory, so its
# pages compete with the foreground for RAM residency (this is what produces
# genuine thrash). A frozen cgroup stops running this loop, so its pages go cold
# and get reclaimed, handing RAM back to the foreground.
s = 0
while True:
    for c in chunks:
        for i in range(0, len(c), PAGE):
            s += c[i]
            c[i] = s & 0xff
