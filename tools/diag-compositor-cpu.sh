#!/usr/bin/env bash
# Diagnose the veracage-compositor busy-loop. Run while Veracage is open and the
# compositor is at ~130% CPU (ideally with an app window mapped). Needs sudo for
# the perf profile (the compositor runs as the veracage uid).
#   sudo bash tools/diag-compositor-cpu.sh
set -u

PID=$(pgrep -f 'bin/veracage-compositor --socket' | head -1)
[ -z "${PID:-}" ] && { echo "compositor not running"; exit 1; }
echo "compositor pid=$PID"
echo "binary:  $(readlink -f /proc/$PID/exe)   (mtime $(stat -c %y /proc/$PID/exe 2>/dev/null))"

echo
echo "=== per-thread CPU (which THREAD is hot: main event loop vs veracage-hostclip worker) ==="
top -H -b -n2 -d1 -p "$PID" 2>/dev/null | awk 'f&&NF{print} /PID +USER/{f=1}'

echo
echo "=== hot thread kernel stacks (spinning in a syscall?) ==="
for t in /proc/$PID/task/*; do
  tid=$(basename "$t")
  echo "-- tid $tid ($(cat "$t/comm" 2>/dev/null)) wchan=$(cat "$t/wchan" 2>/dev/null)"
  cat "$t/stack" 2>/dev/null | head -6
done

echo
echo "=== 5s perf profile: where the CPU goes (functions/libs) ==="
if command -v perf >/dev/null; then
  perf record -F 199 -g -p "$PID" -o /tmp/vc-perf.data -- sleep 5 2>/dev/null
  perf report -i /tmp/vc-perf.data --stdio 2>/dev/null | grep -vE '^#|^$' | head -45
else
  echo "perf not installed (try: sudo apt install linux-perf)"
fi
