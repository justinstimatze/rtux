#!/bin/sh
# Replace zram-in-front-of-disk-swap with zswap over the disk swapfile.
#
# zram at high priority with a disk swapfile behind it fills zram with cold
# pages and sends every later (hot) swap-out to disk: LRU inversion. zswap
# keeps a compressed pool in RAM in front of the same swapfile and, with its
# shrinker on, evicts the coldest pool pages to disk, so hot pages stay in RAM.
# See docs/postmortem-2026-11-20-lockup.md.
#
# Run with sudo. Takes effect now (runtime) and on every boot (GRUB + sysctl).
# Undo: scripts/host-zram-to-zswap.sh --undo
set -eu

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }

GRUB_CFG=/etc/default/grub.d/90-zswap.cfg
ZRAM_CONF=/etc/systemd/zram-generator.conf
SYSCTL_OLD=/etc/sysctl.d/99-zram.conf
SYSCTL_NEW=/etc/sysctl.d/99-zswap.conf
POOL_PCT=25

if [ "${1:-}" = "--undo" ]; then
    rm -f "$GRUB_CFG" "$SYSCTL_NEW" "$ZRAM_CONF"
    [ -f "$ZRAM_CONF.disabled" ] && mv "$ZRAM_CONF.disabled" "$ZRAM_CONF"
    [ -f "$SYSCTL_OLD.disabled" ] && mv "$SYSCTL_OLD.disabled" "$SYSCTL_OLD"
    update-grub
    echo "Restored zram config. Reboot to bring zram back."
    exit 0
fi

# 1. Persist: zswap on at boot via the kernel command line.
cat > "$GRUB_CFG" <<EOF
GRUB_CMDLINE_LINUX_DEFAULT="\$GRUB_CMDLINE_LINUX_DEFAULT zswap.enabled=1 zswap.compressor=zstd zswap.max_pool_percent=$POOL_PCT zswap.shrinker_enabled=1"
EOF
update-grub

# 2. Persist: stop zram-generator creating zram0. Moving our config aside is not
#    enough: Ubuntu ships /usr/lib/systemd/zram-generator.conf with a [zram0]
#    section, and systemd re-creates the device from it within seconds of a
#    swapoff. An empty file in /etc overrides the /usr/lib default.
if [ -s "$ZRAM_CONF" ] && [ ! -f "$ZRAM_CONF.disabled" ]; then
    mv "$ZRAM_CONF" "$ZRAM_CONF.disabled"
fi
printf '# Empty on purpose: zswap replaces zram. See rtux scripts/host-zram-to-zswap.sh\n' > "$ZRAM_CONF"
systemctl daemon-reload

# 3. Persist: swappiness 180 was tuned for zram, where a swap-out costs only
#    CPU. With disk behind the pool, use 100 (equal weight anon vs file).
[ -f "$SYSCTL_OLD" ] && mv "$SYSCTL_OLD" "$SYSCTL_OLD.disabled"
cat > "$SYSCTL_NEW" <<EOF
# zswap in front of /swapfile; see rtux scripts/host-zram-to-zswap.sh
vm.swappiness = 100
EOF
# Apply the way boot does (every file, name order, last write wins) and check
# that ours won, so a later-sorting file can't quietly keep an old value.
sysctl --system >/dev/null
got=$(sysctl -n vm.swappiness)
if [ "$got" != 100 ]; then
    echo "WARNING: vm.swappiness is $got, not 100. Another sysctl file overrides ours:" >&2
    grep -rn 'vm\.swappiness' /etc/sysctl.conf /etc/sysctl.d/ /run/sysctl.d/ /usr/lib/sysctl.d/ 2>/dev/null >&2
fi
sysctl -w vm.page-cluster=3   # kernel default; 0 was for zram's random access

# 4. Runtime: turn zswap on now.
echo zstd > /sys/module/zswap/parameters/compressor
echo $POOL_PCT > /sys/module/zswap/parameters/max_pool_percent
echo Y > /sys/module/zswap/parameters/shrinker_enabled
echo Y > /sys/module/zswap/parameters/enabled

# 5. Runtime: drain zram, but only if RAM can take its pages back.
if grep -q '^/dev/zram0' /proc/swaps; then
    used_kb=$(awk '$1=="/dev/zram0"{print $4}' /proc/swaps)
    avail_kb=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)
    if [ "$avail_kb" -gt $((used_kb * 2)) ]; then
        swapoff /dev/zram0
        systemctl stop systemd-zram-setup@zram0.service 2>/dev/null || true
        echo "zram0 drained (${used_kb} kB moved back)."
    else
        echo "Left zram0 on: ${used_kb} kB in use vs ${avail_kb} kB available. It goes away at next reboot."
    fi
fi

echo
grep -H . /sys/module/zswap/parameters/enabled /sys/module/zswap/parameters/compressor \
    /sys/module/zswap/parameters/max_pool_percent /sys/module/zswap/parameters/shrinker_enabled
cat /proc/swaps
