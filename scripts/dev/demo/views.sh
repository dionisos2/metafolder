# shellcheck shell=bash
# The views scripts/dev/demo.sh screenshots, one function each, sourced by it
# once the GUI is up: it defines mfg (mf gui on this GUI), REPO (the
# demonstration repository's uuid), close_initial (closes the GUI's first,
# empty workspace) and DISPLAY (the private X server).
#
#   shoot_all <out dir>   every view, as <out dir>/<view>.png
#
# A view is a workspace of its own, built by commands only (`mf gui command`,
# workspace variables): no click, no key, nothing at coordinates, so a change of
# style changes the picture, never what it shows.

# A README shows a screenshot at about 900 px wide: the window is drawn at
# 1440 × 900 and kept so, sharp enough when scaled down, the text still legible.
DEMO_WIDTH=1440
DEMO_HEIGHT=900

# view_workspace <name> <left panel> [<right panel>] — a new workspace on the
# repository, alone on screen, showing these panels; prints its id.
view_workspace() {
    local ws
    ws=$(mfg workspace new --repo "$REPO" --left "$2") || return 1
    mfg layout left "$ws" >/dev/null || return 1
    if [ -n "${3:-}" ]; then
        mfg layout right "$ws" >/dev/null && mfg view right "$3" >/dev/null || return 1
    else
        mfg layout right - >/dev/null || return 1
    fi
    mfg command panel:focus left >/dev/null || return 1
    mfg command workspace:rename "$1" >/dev/null || return 1
    printf '%s\n' "$ws"
}

# send <workspace> <variable> <value> — writes a workspace variable.
send() { mfg command workspace:send "$1" "$2" "$3" >/dev/null; }

# capture <file> — the screen, once it has stopped changing: thumbnails and
# previews load after the commands return, for a time no duration bounds. Two
# identical captures in a row are taken as settled.
capture() {
    local file=$1 previous="" current
    for _ in $(seq 1 60); do
        import -window root "$file.tmp.png" || return 1
        current=$(magick "$file.tmp.png" -depth 8 rgba:- | sha256sum)
        [ "$current" = "$previous" ] && break
        previous=$current
        sleep 0.5
    done
    # 256 colours: a quarter of the size, the thumbnails barely changed.
    magick "$file.tmp.png" -strip -colors 256 -define png:compression-level=9 "$file" || return 1
    rm -f "$file.tmp.png"
}

# The photos of the trips, as thumbnails, and one of them previewed.
view_photos() {
    local ws
    ws=$(view_workspace Photos metarecord-list viewer) || return 1
    send "$ws" metarecord-list:mode grid
    send "$ws" metarecord-list:query '#=lieu'
}

# The tag hierarchy walked down to nature/animal, and the files under it with
# their tags, ratings and comments.
view_tags() {
    local ws
    ws=$(view_workspace Tags path-ref metarecord-list) || return 1
    mfg command path-ref:set field path >/dev/null || return 1
    mfg command cursor:find self select nature >/dev/null || return 1
    mfg command cursor:descend self >/dev/null || return 1
    mfg command cursor:find self select animal >/dev/null || return 1
    mfg command path-ref:toggle-scope >/dev/null || return 1
    send "$ws" metarecord-list:columns '["mfr_path:value","tag>path:path","rating","comment"]'
    send "$ws" metarecord-list:query '#=nature/animal'
}

# What was written, step by step, each one a label that can be undone; beside
# it, the files annotated in the last step.
view_log() {
    local ws
    ws=$(view_workspace Historique metarecord-list log) || return 1
    send "$ws" metarecord-list:columns '["mfr_path:value","comment","rating"]'
    send "$ws" metarecord-list:query 'comment?'
}

shoot_all() {
    local out=$1 win view
    win=$(xdotool search --onlyvisible --class metafolder | head -n 1)
    [ -n "$win" ] || { echo "error: no GUI window on $DISPLAY" >&2; return 1; }
    xdotool windowmove "$win" 0 0 windowsize --sync "$win" "$DEMO_WIDTH" "$DEMO_HEIGHT" || return 1
    for view in photos tags log; do
        "view_$view" || { echo "error: the view $view failed" >&2; return 1; }
        # In this shell, not in a view's $(…): the GUI reuses a closed
        # workspace's id, so the initial one must be closed exactly once.
        close_initial
        capture "$out/demo-$view.png" || return 1
        echo "  $out/demo-$view.png"
    done
}
