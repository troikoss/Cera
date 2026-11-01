#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Runs drive.py steps in the running headless session and shows the end of Cera's log:
#   run.sh 'click(80, 107); sleep(1); shot("tree")'      screenshots land in $CERA_HEADLESS
HERE=$(cd "$(dirname "$0")" && pwd)
H=${CERA_HEADLESS:-$HOME/.cache/cera-headless}
export DBUS_SESSION_BUS_ADDRESS=$(cat $H/bus) XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
python3 $HERE/drive.py "$1" 2>&1 | tail -3
tail -${2:-4} $H/cera.log | cut -c1-170
