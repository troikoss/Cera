#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Renderer comparison in a headless 2560x1600 session: the scripted demo (draw, erase, undo,
# then a 5 s scroll at 4000 px/s) on a copy of NOTE, per GSK renderer.
#   bench.sh note.cera
NOTE=${1:?usage: bench.sh note.cera}
HERE=$(cd "$(dirname "$0")" && pwd)
# State (bus socket, logs, screenshots) and the binary to test.
H=${CERA_HEADLESS:-$HOME/.cache/cera-headless}
CERA_BIN=${CERA_BIN:-$HERE/../../target/release/cera}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
$HERE/session.sh stop >/dev/null 2>&1
$HERE/session.sh start 2560x1600 none >/dev/null 2>&1
export DBUS_SESSION_BUS_ADDRESS=$(cat $H/bus) GIO_USE_VFS=local GTK_A11Y=none GDK_DEBUG=no-portals
for r in vulkan gl cairo; do
    cp "$NOTE" $H/bench.cera
    rm -rf $H/bench-config
    env WAYLAND_DISPLAY=cera-test GDK_BACKEND=wayland XDG_CONFIG_HOME=$H/bench-config CERA_DEBUG=1 \
        CERA_DEBUG_DEMO=1 RUST_LOG=cera=debug GSK_RENDERER=$r $CERA_BIN $H/bench.cera \
        > $H/bench-$r.log 2>&1 &
    P=$!
    for i in $(seq 90); do grep -q "demo: done" $H/bench-$r.log && break; sleep 1; done
    sleep 1
    RSS=$(grep -E "^Rss:" /proc/$P/smaps_rollup | awk '{print int($2/1024)}')
    kill $P; wait $P 2>/dev/null
    echo "== $r: RSS ${RSS} MiB"
    sed -n "/demo: scrolling/,/demo: done/p" $H/bench-$r.log | grep -oE "frame [0-9.]+ ms avg, [0-9.]+ max|demo: done.*" | head -12
done
$HERE/session.sh stop
