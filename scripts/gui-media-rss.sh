#!/usr/bin/env bash
# Drive the running metafolder GUI's `file` panel to a media file and monitor
# the WebKitWebProcess RSS/CPU. This is the exact harness used to debug the
# video-preview memory blow-up.
#
# It pushes `selected_paths` to a file through the GUI scripting API
# (PUT /gui/panels/<slot>/view) — the same signal the `file` panel reacts to —
# then samples the WebKit web process once a second. The bug shows up as RSS
# climbing into the gigabytes, and/or the web-process PID changing (a crash +
# shell reload).
#
# WHY THIS MAY DISAGREE WITH MANUAL TESTING:
#   Setting the view only makes the panel SHOW the file — it loads the first
#   frame (preload="metadata"). It does NOT press Play. If the blow-up only
#   happens once playback starts, that is the finding: start the monitor here,
#   then click Play in the GUI window and watch RSS jump in this output.
#
# Prerequisites:
#   - the GUI is already running (start it yourself: `metafolder-gui`);
#   - the panels in ~/.config/metafolder/gui/ are up to date (run
#     metafolder-sync-config after editing the shipped panel sources).
#
# Usage:
#   scripts/gui-media-rss.sh <file> [<file> ...]
#     one file   -> show it, then monitor for DWELL seconds;
#     many files -> cycle through them (ROUNDS rounds, DWELL seconds each) to
#                   exercise the teardown that runs on every view change.
#
# Tunables (env):
#   GUI_URL  base URL of the GUI server   (default http://127.0.0.1:7524)
#   SLOT     left | right                 (default left)
#   DWELL    seconds monitored per file   (default 20)
#   ROUNDS   passes over the file list    (default 1)

set -euo pipefail

GUI_URL=${GUI_URL:-http://127.0.0.1:7524}
SLOT=${SLOT:-left}
DWELL=${DWELL:-20}
ROUNDS=${ROUNDS:-1}

die() { echo "error: $*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: $0 <file> [<file> ...]"
command -v curl >/dev/null    || die "curl is required"
command -v python3 >/dev/null || die "python3 is required (for safe JSON encoding)"
curl -sf -m 3 "$GUI_URL/gui/status" >/dev/null \
  || die "no GUI responding at $GUI_URL — start it with: metafolder-gui"

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

# One line summarising every WebKitWebProcess: total RSS (MB), total CPU (%),
# and the PID set — a changed PID set means the web process crashed and the
# shell reloaded.
sample() {
  ps -C WebKitWebProcess -o pid=,rss=,pcpu= 2>/dev/null | awk '
    { pids = pids $1 ","; rss += $2; cpu += $3 }
    END { printf "RSS=%dMB  CPU=%.0f%%  webproc_pids=[%s]", rss/1024, cpu, pids }'
}

monitor() {
  local label=$1 secs=$2 t=0
  while [ "$t" -lt "$secs" ]; do
    printf '  %-30s t=%2ds  %s\n' "$label" "$t" "$(sample)"
    sleep 1; t=$((t + 1))
  done
}

echo "GUI=$GUI_URL  slot=$SLOT  dwell=${DWELL}s  rounds=$ROUNDS"
echo "baseline: $(sample)"
echo "note: set-view loads the first frame only; click Play in the window to test playback."
echo

for _ in $(seq 1 "$ROUNDS"); do
  for f in "$@"; do
    [ -e "$f" ] || die "no such file: $f"
    shown=$(show "$f")
    echo "→ shown: $shown"
    monitor "$(basename "$shown")" "$DWELL"
    echo
  done
done
