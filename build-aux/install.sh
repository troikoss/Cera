#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs Cera into $PREFIX (default ~/.local): the binary, desktop entry, AppStream
# metainfo, icons and the .cera MIME type. Build first: cargo build --release -p cera
#
#   build-aux/install.sh                 install for this user
#   sudo PREFIX=/usr/local build-aux/install.sh
#   build-aux/install.sh --uninstall
set -eu

PREFIX=${PREFIX:-$HOME/.local}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA=$ROOT/crates/app/data
BIN=${CERA_BIN:-$ROOT/target/release/cera}
ID=io.github.troikoss.Cera

files="
bin/cera:$BIN:755
share/applications/$ID.desktop:$DATA/$ID.desktop:644
share/metainfo/$ID.metainfo.xml:$DATA/$ID.metainfo.xml:644
share/mime/packages/$ID.xml:$DATA/$ID.mime.xml:644
share/icons/hicolor/scalable/apps/$ID.svg:$DATA/icons/scalable/apps/$ID.svg:644
share/icons/hicolor/symbolic/apps/$ID-symbolic.svg:$DATA/icons/symbolic/apps/$ID-symbolic.svg:644
"

for entry in $files; do
    dest=$PREFIX/${entry%%:*}
    rest=${entry#*:}
    src=${rest%:*}
    mode=${rest##*:}
    if [ "${1:-}" = "--uninstall" ]; then
        rm -f "$dest"
    else
        install -Dm"$mode" "$src" "$dest"
    fi
done

# Flatpak builds export these files and build the caches themselves.
if [ -n "${FLATPAK_ID:-}" ]; then
    exit 0
fi

# Refresh the caches that list MIME types, apps and icons; harmless where the tools are missing.
if command -v update-mime-database >/dev/null 2>&1; then
    update-mime-database "$PREFIX/share/mime" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$PREFIX/share/applications" || true
fi
if command -v gtk4-update-icon-cache >/dev/null 2>&1; then
    gtk4-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" || true
elif command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$PREFIX/share/icons/hicolor" || true
fi
