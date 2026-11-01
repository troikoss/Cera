# SPDX-License-Identifier: GPL-3.0-or-later
"""Starts a private session bus with dbus-broker (Fedora has no dbus-daemon): binds a socket
and hands it over as systemd socket activation expects. Prints the bus address."""
import os, socket, sys
path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.bind(path)
sock.listen(64)
pid = os.fork()
if pid == 0:
    os.setsid()
    if sock.fileno() != 3:
        os.dup2(sock.fileno(), 3)
    os.set_inheritable(3, True)  # dup2 onto itself would leave close-on-exec set
    os.environ["LISTEN_FDS"] = "1"
    os.environ["LISTEN_PID"] = str(os.getpid())
    devnull = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(devnull, fd)
    os.execvp("dbus-broker-launch", ["dbus-broker-launch", "--scope", "user"])
print(pid, f"unix:path={path}")
