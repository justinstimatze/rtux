#!/bin/bash
# setup-zram.sh — give this box macOS/Fedora-style compressed RAM swap.
# Run as root:  sudo ./setup-zram.sh
#
# What it does:
#   - installs systemd-zram-generator (the modern zram setup, same as Fedora)
#   - creates a zstd-compressed zram swap device sized to ~half your RAM
#   - tunes the kernel to prefer fast zram over the slow disk swapfile
#   - leaves your existing /swapfile as a low-priority last-resort overflow
# Reversible: `sudo swapoff /dev/zram0`, delete /etc/systemd/zram-generator.conf
# and /etc/sysctl.d/99-zram.conf, then remove the systemd-zram-generator package
# (apt/dnf/pacman). `sudo ./uninstall.sh` does all of this for you.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: run as root:  sudo ./setup-zram.sh"
    exit 1
fi

echo "== installing systemd-zram-generator =="
if command -v apt-get >/dev/null; then
    apt-get update -qq
    apt-get install -y systemd-zram-generator
elif command -v dnf >/dev/null; then
    # Fedora ships zram on by default via zram-generator-defaults, but install
    # the package in case it was removed.
    dnf install -y zram-generator || true
elif command -v pacman >/dev/null; then
    pacman -S --needed --noconfirm zram-generator
else
    echo "This auto-setup installs the package via apt/dnf/pacman, none of which"
    echo "were found. Install 'systemd-zram-generator' (or 'zram-generator') with"
    echo "your package manager, then re-run this script — it will still write the"
    echo "config and tuning below."
    exit 0
fi

echo "== writing /etc/systemd/zram-generator.conf =="
cat > /etc/systemd/zram-generator.conf <<'EOF'
# Compressed RAM swap. zstd compresses typical anon pages ~2.5-3x, so this
# ~7 GiB device holds ~18 GiB of swapped pages while costing ~2.5 GiB of real RAM.
[zram0]
zram-size = ram / 2
compression-algorithm = zstd
# High priority so the kernel reaches for fast zram before the disk swapfile.
swap-priority = 100
EOF

echo "== tuning kernel for zram (fast swap) =="
cat > /etc/sysctl.d/99-zram.conf <<'EOF'
# With fast (RAM) swap, it pays to swap cold pages eagerly instead of thrashing
# the working set — the opposite of the low value tuned for slow disk swap.
vm.swappiness = 180
# zram is random-access RAM; read-ahead just wastes work.
vm.page-cluster = 0
EOF
sysctl --system >/dev/null

echo "== (re)applying zram config =="
# `apt install` may have already created zram0 at package defaults (4G/lzo-rle)
# BEFORE our config existed. Tear the live device down so our size + zstd
# actually take effect, then let the generator recreate it from our config.
swapoff /dev/zram0 2>/dev/null || true
systemctl daemon-reload
systemctl restart systemd-zram-setup@zram0.service

echo
echo "== result =="
swapon --show
echo
echo "Done. zram0 should appear above /swapfile with PRIO 100."
echo "Verify compression live with:  zramctl"
