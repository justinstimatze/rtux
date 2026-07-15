#!/usr/bin/env bash
# watch-faultin.sh — show what fault-in and the bulk ceiling actually did.
# Run after `sudo ./install.sh`. Needs no root.
#
# NOTE: this script previously grepped the ENTIRE journal, so it replayed log
# lines from earlier daemon runs and presented them as current — which is how a
# stale "mechanism broken" verdict survived a rebuild that fixed it. It now
# (a) refuses to report if the installed binary isn't the one just built, and
# (b) only reads the journal since the current daemon start.

U=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service
A=$U/app.slice
S=$U/session.slice
REPO=$(cd "$(dirname "$0")/.." && pwd)
mb() { echo "scale=1; ${1:-0}/1048576" | bc -l; }

echo "=== staleness check ==="
if [ -f "$REPO/target/release/pressured" ] && \
   ! cmp -s "$REPO/target/release/pressured" /usr/local/bin/pressured 2>/dev/null; then
    printf "  \033[31m✗ the running daemon is NOT the freshly built binary\033[0m\n"
    printf "    built:     %s\n" "$(stat -c %y "$REPO/target/release/pressured")"
    printf "    installed: %s\n" "$(stat -c %y /usr/local/bin/pressured)"
    printf "    -> run: sudo ./install.sh   (everything below would be stale)\n"
    exit 1
fi
printf "  \033[32m✓ running binary matches the build\033[0m\n"

# Only look at this daemon run — never replay a previous one's verdict.
SINCE=$(systemctl show -p ActiveEnterTimestamp --value rtux.service)
echo "  daemon up since: $SINCE"

echo
echo "=== bulk ceiling ==="
high=$(cat "$A/memory.high" 2>/dev/null || echo max)
if [ "$high" = "max" ]; then
    printf "  \033[31m✗ still max — set_bulk_ceiling not running\033[0m\n"
else
    printf "  \033[32m✓ app.slice memory.high = %sM\033[0m (current %sM)\n" \
        "$(mb "$high")" "$(mb "$(cat $A/memory.current)")"
fi

echo
echo "=== fault-in, this run only ==="
journalctl -u rtux.service --no-pager -o cat --since "$SINCE" 2>/dev/null \
    | grep -E 'fault-in' | sort -u | head -20 \
    || echo "  (no fault-in lines yet — first pass is ~30s after start)"

echo
echo "=== gnome-shell: how much of its swap is even touchable? ==="
# The only honest discriminator: sum smaps Swap across its pids (pages with a swap
# PTE — the ONLY ones an address touch can pull back) and compare to the cgroup's
# swap counter. The gap is shmem, which no toucher can reach at any budget.
# Do NOT substitute `memory.stat anon` for this — anon counts RESIDENT anon only,
# so a swapped-out anon page is missing from it and the comparison proves nothing.
G=$S/org.gnome.Shell@ubuntu.service
tot=0
for p in $(cat "$G/cgroup.procs" 2>/dev/null); do
    kb=$(awk '/^Swap:/{t+=$2} END{print t+0}' "/proc/$p/smaps" 2>/dev/null) || continue
    tot=$((tot + ${kb:-0}))
done
printf "  smaps Swap (touchable, anon) : %sM\n" "$(mb $((tot * 1024)))"
printf "  memory.swap.current          : %sM\n" "$(mb "$(cat $G/memory.swap.current)")"
printf "  memory.stat shmem (resident) : %sM\n" "$(mb "$(awk '/^shmem /{print $2}' $G/memory.stat)")"
printf "  memory.current               : %sM\n" "$(mb "$(cat $G/memory.current)")"
echo "  (touchable should trend to ~0; the rest is shmem and stays — that's expected)"

echo
echo "=== anon-backed units: these are the ones that should heal ==="
for u in app-gnome-terminator-*.scope; do :; done
for cg in "$S/org.freedesktop.IBus.session.GNOME.service" \
          "$S/wireplumber.service" \
          "$S/dbus.service" \
          $A/app-gnome-terminator-*.scope; do
    [ -r "$cg/memory.swap.current" ] || continue
    printf "  %-40s swap=%7sM\n" "$(basename "$cg" | cut -c1-40)" \
        "$(mb "$(cat $cg/memory.swap.current)")"
done
