#!/bin/sh
# Installs this metafolder release (doc "Building a release").
#
#   ./install.sh [--prefix DIR] [--watchd]
#
# Copies the programs to DIR/bin and the defaults to DIR/share/metafolder/
# (DIR is ~/.local unless given, or $PREFIX), then applies the defaults to
# your configuration, ~/.config/metafolder/, with metafolder-sync-config: run
# again to upgrade, your edits are merged with the new defaults.
#
# --watchd also installs the fanotify broker as a system service, with sudo:
# root-owned under /usr/local/bin, never under DIR, where a binary you can
# rewrite would run with CAP_SYS_ADMIN (doc "The fanotify broker").
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
watchd=no

usage() { echo "usage: $0 [--prefix DIR] [--watchd]"; }
while [ $# -gt 0 ]; do
    case $1 in
        --prefix) [ $# -ge 2 ] || { usage >&2; exit 2; }; prefix=$2; shift 2 ;;
        --watchd) watchd=yes; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option '$1'" >&2; usage >&2; exit 2 ;;
    esac
done

mkdir -p "$prefix/bin" "$prefix/share/metafolder"
for b in metafolder-daemon mf metafolder-gui metafolder-sync-config; do
    echo "install $prefix/bin/$b"
    install -m 0755 "$here/bin/$b" "$prefix/bin/$b"
done
# Replaced whole: a default the new release no longer ships must not stay.
rm -rf "$prefix/share/metafolder/defaults"
cp -R "$here/share/metafolder/defaults" "$prefix/share/metafolder/defaults"
echo "install $prefix/share/metafolder/defaults"

# With no argument it reads the defaults beside itself.
"$prefix/bin/metafolder-sync-config"

if [ "$watchd" = yes ]; then
    sudo install -D -m 0755 "$here/bin/metafolder-watchd" /usr/local/bin/metafolder-watchd
    sudo install -D -m 0644 "$here/share/metafolder/watchd/metafolder-watchd.sysusers" \
        /etc/sysusers.d/metafolder-watchd.conf
    sudo systemd-sysusers /etc/sysusers.d/metafolder-watchd.conf
    sudo install -D -m 0644 "$here/share/metafolder/watchd/metafolder-watchd.service" \
        /etc/systemd/system/metafolder-watchd.service
    sudo systemctl daemon-reload
    sudo systemctl enable metafolder-watchd
    sudo systemctl restart metafolder-watchd
    echo "metafolder-watchd installed; restart the daemon to watch with it."
else
    echo "The fanotify broker is not installed: ./install.sh --watchd adds it (sudo)."
fi
echo "Installed. Ensure $prefix/bin is on your PATH."
