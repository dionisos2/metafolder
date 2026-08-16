#!/usr/bin/env bash
# Checks the build- and run-time dependencies metafolder needs and reports what
# is missing. Grouped by role:
#
#   build            — needed to compile the workspace (all crates, GUI included)
#   runtime (GUI)    — bubblewrap: the GUI refuses to start without it
#   runtime optional — GUI media features; each degrades gracefully when absent
#
# Exit status is 0 unless a *required* dependency (build, or the hard GUI
# runtime dep) is missing; missing optional dependencies only warn. So it is
# safe to gate `make install` on this script.
#
#   scripts/check-deps.sh            # report and set exit status
#   scripts/check-deps.sh --quiet    # only print problems (nothing on success)
#
# Package names in the install hints are for Arch Linux; map them to your
# distribution.
#
# Testing seam: if MF_CHECK_STUB points at a file, any "name=0" line there forces
# that dependency ABSENT and every unlisted dependency is treated as present.
# Used by scripts/test-check-deps.sh; unset in normal use.

set -uo pipefail

quiet=false
for arg in "${@:-}"; do
    case "$arg" in
        --quiet|-q) quiet=true ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        "") ;;
        *) echo "check-deps: unknown option: $arg (try --help)" >&2; exit 2 ;;
    esac
done

if [ -t 1 ]; then
    red=$'\e[31m'; green=$'\e[32m'; yellow=$'\e[33m'; bold=$'\e[1m'; off=$'\e[0m'
else
    red=''; green=''; yellow=''; bold=''; off=''
fi

# The dependency table: one row per dependency.
#   name | category | probe | arg | arch-package | purpose
# category: build | runtime | optional
# probe:    cmd (command -v arg) | pc (pkg-config --exists arg)
#         | emoji (an emoji font in fontconfig) | gst (gst-inspect element)
deps=(
    "cargo|build|cmd|cargo|rust|Rust toolchain (compiler + cargo)"
    "cc|build|cmd|cc|base-devel|C toolchain (bundled SQLite/libgit2)"
    "pkg-config|build|cmd|pkg-config|base-devel|locate system libraries"
    "node|build|cmd|node|nodejs|build the GUI frontend bundle"
    "npm|build|cmd|npm|npm|build the GUI frontend bundle"
    "openssl|build|pc|openssl|openssl|linked by reqwest (gui/bench)"
    "webkit2gtk|build|pc|webkit2gtk-4.1|webkit2gtk-4.1|Tauri WebView (gui crate)"
    "gtk3|build|pc|gtk+-3.0|gtk3|Tauri windowing (gui crate)"
    "librsvg|build|pc|librsvg-2.0|librsvg|Tauri icon rendering (gui crate)"
    "bwrap|runtime|cmd|bwrap|bubblewrap|sandbox — the GUI refuses to start without it"
    "emoji-font|optional|emoji||noto-fonts-emoji|file-manager / thumbnail glyphs"
    "ffmpeg|optional|cmd|ffmpeg|ffmpeg|video/GIF poster thumbnails"
    "gst-discoverer|optional|cmd|gst-discoverer-1.0|gst-plugins-base|per-file codec probe"
    "gst-sink|optional|gst|autoaudiosink|gst-plugins-good|audio/video sinks WebKit needs"
    "gst-h264|optional|gst|avdec_h264|gst-libav|H.264 decode for inline preview"
)

# present <probe> <arg> <name> — true if the dependency is available.
present() {
    local probe=$1 arg=$2 name=$3
    if [ -n "${MF_CHECK_STUB:-}" ]; then
        # Listed as absent ⇒ missing; anything else ⇒ present.
        ! grep -qx "${name}=0" "$MF_CHECK_STUB"
        return
    fi
    case "$probe" in
        cmd)   command -v "$arg" >/dev/null 2>&1 ;;
        pc)    command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "$arg" 2>/dev/null ;;
        # grep without -q so it reads all of fc-list's output: with -q it would
        # close the pipe on the first match and, under `set -o pipefail`, the
        # SIGPIPE'd fc-list (exit 141) would fail the whole probe on machines
        # with many fonts installed.
        emoji) command -v fc-list >/dev/null 2>&1 && fc-list 2>/dev/null | grep -i emoji >/dev/null ;;
        gst)   command -v gst-inspect-1.0 >/dev/null 2>&1 && gst-inspect-1.0 "$arg" >/dev/null 2>&1 ;;
        *)     return 1 ;;
    esac
}

# Missing package sets, per category, in first-seen order (deduplicated).
declare -a miss_build miss_runtime miss_optional

section() {
    local want=$1 title=$2
    local shown=false row name category probe arg pkg purpose
    for row in "${deps[@]}"; do
        IFS='|' read -r name category probe arg pkg purpose <<<"$row"
        [ "$category" = "$want" ] || continue
        if present "$probe" "$arg" "$name"; then
            $quiet || {
                $shown || { printf '%s%s%s\n' "$bold" "$title" "$off"; shown=true; }
                printf '  %s✓%s %-16s %s\n' "$green" "$off" "$name" "$purpose"
            }
        else
            $shown || { $quiet || printf '%s%s%s\n' "$bold" "$title" "$off"; shown=true; }
            printf '  %s✗%s %-16s %s%s%s\n' "$red" "$off" "$name" "$yellow" "$purpose" "$off"
            case "$want" in
                build)    miss_build+=("$pkg") ;;
                runtime)  miss_runtime+=("$pkg") ;;
                optional) miss_optional+=("$pkg") ;;
            esac
        fi
    done
    $quiet || { $shown && echo; }
}

# uniq_words <array...> — echo the arguments with duplicates removed, order kept.
uniq_words() {
    local seen=" " w
    for w in "$@"; do
        case "$seen" in *" $w "*) ;; *) printf '%s ' "$w"; seen="$seen$w " ;; esac
    done
}

$quiet || printf '%smetafolder — dependency check%s\n\n' "$bold" "$off"

section build    "Build dependencies (required)"
section runtime  "Runtime, GUI (required)"
section optional "Runtime, GUI (optional — media features)"

# ── verdict ──────────────────────────────────────────────────────────────────
status=0
required_pkgs=$(uniq_words "${miss_build[@]}" "${miss_runtime[@]}")
optional_pkgs=$(uniq_words "${miss_optional[@]}")

if [ -n "$required_pkgs" ]; then
    status=1
    printf '%s%sMissing required dependencies.%s Install with:\n' "$bold" "$red" "$off"
    printf '  %spacman -S --needed %s%s\n' "$bold" "$required_pkgs" "$off"
fi
if [ -n "$optional_pkgs" ]; then
    printf '%sOptional (GUI media) not found%s — install for full functionality:\n' "$yellow" "$off"
    printf '  %spacman -S --needed %s%s\n' "$bold" "$optional_pkgs" "$off"
fi
if [ "$status" -eq 0 ] && [ -z "$optional_pkgs" ]; then
    $quiet || printf '%s%sall dependencies present%s\n' "$bold" "$green" "$off"
elif [ "$status" -eq 0 ]; then
    $quiet || printf '%sall required dependencies present%s\n' "$green" "$off"
fi

exit "$status"
