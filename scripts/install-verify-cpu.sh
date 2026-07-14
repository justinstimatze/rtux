#!/bin/bash
# Install the CPU-protection daemon and verify the cpu controller + weights.
# Run with: sudo bash scripts/install-verify-cpu.sh
set -u
REPO=/home/gas6amus/Documents/rtux
U=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service
S=$U/session.slice

echo "== installing $(${REPO}/target/release/pressured --version) =="
install -m755 "$REPO/target/release/pressured" /usr/local/bin/pressured
systemctl restart rtux.service
echo "restarted; waiting 35s for protection to land..."
sleep 35

echo
echo "== cpu controller enabled in the subtree? (want 'cpu' present) =="
for d in "$U" "$S" "$U/app.slice"; do
  echo "  $(basename $d) subtree_control: $(cat $d/cgroup.subtree_control 2>/dev/null)"
done

echo
echo "== desktop slice weight (want 1000) + a few leaf weights =="
echo "  session.slice cpu.weight = $(cat $S/cpu.weight 2>/dev/null || echo 'n/a — controller not enabled')"
echo "  app.slice     cpu.weight = $(cat $U/app.slice/cpu.weight 2>/dev/null || echo n/a)"
echo "  gnome-shell   cpu.weight = $(cat $S/org.gnome.Shell@ubuntu.service/cpu.weight 2>/dev/null || echo n/a)"

echo
echo "== foreground boost: focused app should read 1000 (needs the extension"
echo "   reporting focus; click a window, then re-run to see it move) =="
found=0
for scope in $(find "$U/app.slice" -maxdepth 2 -name cpu.weight 2>/dev/null); do
  w=$(cat "$scope" 2>/dev/null)
  if [ "$w" = 1000 ]; then echo "  boosted: $(basename $(dirname $scope)) = $w"; found=1; fi
done
[ "$found" = 0 ] && echo "  (no app-slice leaf at 1000 yet — focus a window and re-check)"
