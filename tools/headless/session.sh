#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Starts or stops a private headless mutter (own D-Bus, own Wayland display "cera-test",
# a virtual monitor, the real GPU) with Cera inside, for scripted input through drive.py.
# Nothing appears on the screen and the desktop session is not touched.
#
#   session.sh start [WxH] [renderer] [VAR=value...]    CERA_ARGS: files to open
#   session.sh stop
HERE=$(cd "$(dirname "$0")" && pwd)
# State (bus socket, logs, screenshots) and the binary to test.
H=${CERA_HEADLESS:-$HOME/.cache/cera-headless}
CERA_BIN=${CERA_BIN:-$HERE/../../target/release/cera}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
mkdir -p $H
if [ "$1" = stop ]; then
    [ -f $H/pids ] && kill $(cat $H/pids) 2>/dev/null
    rm -f $H/pids
    exit 0
fi
SIZE=${2:-1280x800}
RENDERER=${3:-vulkan}
shift 3 2>/dev/null
read BUSPID ADDR < <(python3 $HERE/bus.py $H/bus.sock)
echo "$ADDR" > $H/bus
echo $BUSPID > $H/dbus.pid
export DBUS_SESSION_BUS_ADDRESS=$ADDR
sleep 0.5
export GIO_USE_VFS=local NO_AT_BRIDGE=1 GTK_A11Y=none GDK_DEBUG=no-portals
rm -f $XDG_RUNTIME_DIR/cera-test
mutter --headless --no-x11 --virtual-monitor $SIZE --wayland-display cera-test > $H/mutter.log 2>&1 &
echo $! > $H/pids
cat $H/dbus.pid >> $H/pids
# Mutter waits out a few D-Bus activation timeouts on the private bus before it is ready.
for i in $(seq 90); do
    gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.ListNames 2>/dev/null | grep -q RemoteDesktop && break
    sleep 1
done
# Renderer "none": just the compositor; start Cera yourself.
[ "$RENDERER" = none ] && exit 0
env WAYLAND_DISPLAY=cera-test GDK_BACKEND=wayland XDG_CONFIG_HOME=$H/config CERA_DEBUG=1 \
    RUST_LOG=cera=debug GSK_RENDERER=$RENDERER "$@" $CERA_BIN $CERA_ARGS > $H/cera.log 2>&1 &
echo $! >> $H/pids
sleep 3
tail -3 $H/mutter.log; tail -3 $H/cera.log
