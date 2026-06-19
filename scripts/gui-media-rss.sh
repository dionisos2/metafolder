#!/usr/bin/env bash
# Drive the running metafolder GUI's `file` panel across files and monitor the
# WebKitWebProcess RSS/CPU. This is the harness used to debug the video-preview
# slowdown.
#
# It pushes `selected_paths` to each file in turn through the GUI scripting API
# (PUT /gui/panels/<slot>/view) — the same signal the `file` panel reacts to —
# and samples the WebKit web process. The reported symptom is a stall *at the
# moment of switching away from a video*, so the harness samples FINELY (4x/s)
# right after every change to catch the transient, then coarsely afterwards.
# A change of the web-process PID set means the process crashed and the shell
# reloaded.
#
# WHY EARLIER RUNS MISSED IT:
#   Sampling only the steady state after settling on a file hides the spike
#   that happens *during* the switch. The burst window below is the fix.
#
# The interesting transition is video -> other file, so pass the video FIRST
# and the file you switch to SECOND, e.g.:
#   scripts/gui-media-rss.sh \
#     "benchmarks/real/Apprendre le langage Rust #1 (nouveau) [Vqm_oyT8RBA].mkv" \
#     "benchmarks/real/3-adorable-...png"
# With ROUNDS>1 it ping-pongs between them, exercising the switch repeatedly.
#
# Prerequisites:
#   - the GUI is already running (start it yourself: `metafolder-gui`);
#   - the panels in ~/.config/metafolder/gui/ are up to date (run
#     metafolder-sync-config after editing the shipped panel sources).
#
# Usage:
#   scripts/gui-media-rss.sh <file> [<file> ...]
#
# Tunables (env):
#   GUI_URL     base URL of the GUI server     (default http://127.0.0.1:7524)
#   SLOT        left | right                   (default left)
#   BURST_SECS  fine-sampling window per switch (default 4, at 0.25s steps)
#   DWELL       coarse seconds after the burst  (default 8, at 1s steps)
#   ROUNDS      passes over the file list       (default 2)

set -euo pipefail

GUI_URL=${GUI_URL:-http://127.0.0.1:7524}
SLOT=${SLOT:-left}
BURST_SECS=${BURST_SECS:-4}
DWELL=${DWELL:-8}
ROUNDS=${ROUNDS:-2}

die() { echo "error: $*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: $0 <file> [<file> ...]"
command -v curl >/dev/null    || die "curl is required"
command -v python3 >/dev/null || die "python3 is required (for safe JSON encoding)"
curl -sf -m 3 "$GUI_URL/gui/status" >/dev/null \
  || die "no GUI responding at $GUI_URL — start it with: metafolder-gui"
[ $# -ge 2 ] || echo "note: pass a second file to reproduce the video->switch transition" >&2

# JSON-encode the view body safely (paths may contain spaces, #, [], (), unicode).
json_view() { python3 -c 'import json,sys; print(json.dumps({"type":"file","path":sys.argv[1]}))' "$1"; }

# Push a file into the file panel of the chosen slot. Returns the absolute path.
show() {
  local abs; abs=$(readlink -f -- "$1") || abs=$1
  curl -sf -m 5 -X PUT "$GUI_URL/gui/panels/$SLOT/view" \
       -H 'content-type: application/json' --data "$(json_view "$abs")" >/dev/null \
    || die "PUT /gui/panels/$SLOT/view failed for: $abs"
  printf '%s' "$abs"
}

# Print "<rss_mb> <cpu_pct_rounded> <pidset>" for all WebKitWebProcess instances.
sample() {
  ps -C WebKitWebProcess -o pid=,rss=,pcpu= 2>/dev/null | awk '
    { pids = pids $1 ","; rss += $2; cpu += $3 }
    END { printf "%d %.0f %s", rss/1024, cpu, (pids == "" ? "-" : pids) }'
}

prev_pids=""
peak_rss=0
peak_cpu=0
crashes=0

emit() { # emit <label> <t>
  local label=$1 t=$2 rss cpu pids alert=""
  read -r rss cpu pids < <(sample)
  [ "$rss" -gt "$peak_rss" ] && peak_rss=$rss
  [ "$cpu" -gt "$peak_cpu" ] && peak_cpu=$cpu
  if [ -n "$prev_pids" ] && [ "$pids" != "$prev_pids" ] && [ "$pids" != "-" ]; then
    alert="   *** WEB PROCESS PID CHANGED ($prev_pids -> $pids): crash + reload ***"
    crashes=$((crashes + 1))
  fi
  [ "$pids" != "-" ] && prev_pids=$pids
  printf '  %-22s t=%6ss  RSS=%5dMB  CPU=%3s%%  pids=[%s]%s\n' "$label" "$t" "$rss" "$cpu" "$pids" "$alert"
}

# Fine burst (0.25s) then coarse (1s) sampling.
watch_after_change() { # watch_after_change <label>
  local label=$1 i n
  n=$(awk "BEGIN{print int($BURST_SECS/0.25)}")
  for ((i = 1; i <= n; i++)); do emit "$label" "$(awk "BEGIN{printf \"%.2f\", $i*0.25}")"; sleep 0.25; done
  for ((i = 1; i <= DWELL; i++)); do emit "$label" "$((BURST_SECS + i))"; sleep 1; done
}

echo "GUI=$GUI_URL  slot=$SLOT  burst=${BURST_SECS}s@0.25  dwell=${DWELL}s@1  rounds=$ROUNDS"
read -r rss cpu pids < <(sample); prev_pids=$pids
echo "baseline: RSS=${rss}MB CPU=${cpu}% pids=[$pids]"
echo "note: set-view loads the first frame only (preload=metadata); it does not press Play."
echo

for ((r = 1; r <= ROUNDS; r++)); do
  for f in "$@"; do
    [ -e "$f" ] || die "no such file: $f"
    shown=$(show "$f")
    echo "── CHANGE → $(basename "$shown")  (round $r)"
    watch_after_change "$(basename "$shown")"
  done
done

echo
echo "summary: peak RSS=${peak_rss}MB  peak CPU=${peak_cpu}%  web-process crashes=${crashes}"
