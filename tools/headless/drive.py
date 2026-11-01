# SPDX-License-Identifier: GPL-3.0-or-later
"""Drives a headless mutter session (session.sh): pointer, keys and touch through mutter's
RemoteDesktop D-Bus API, plus Cera's CERA_DEBUG actions. Run with DBUS_SESSION_BUS_ADDRESS
set to the session's bus (run.sh does that).

    python3 drive.py 'click(640, 400); touch([line(600, 500, 600, 200)]); shot("a")'

shot() saves Cera's window (dialogs, not popovers); screen() grabs the whole screen from the
screen-cast stream, popovers included. The stylus can't be injected: neither RemoteDesktop
nor libei has tablet tools.
"""
import atexit, os, subprocess, sys, time
import gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION)
RD, SC = "org.gnome.Mutter.RemoteDesktop", "org.gnome.Mutter.ScreenCast"
OUT = os.environ.get("CERA_HEADLESS", os.path.expanduser("~/.cache/cera-headless"))


def call(dest, path, iface, method, args=None):
    r = bus.call_sync(dest, path, iface, method, args, None, Gio.DBusCallFlags.NONE, -1, None)
    return r.unpack() if r is not None else None


session = call(RD, "/org/gnome/Mutter/RemoteDesktop", RD, "CreateSession")[0]
sid = call(RD, session, "org.freedesktop.DBus.Properties", "Get",
           GLib.Variant("(ss)", (RD + ".Session", "SessionId")))[0]
sc = call(SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession",
          GLib.Variant("(a{sv})", ({"remote-desktop-session-id": GLib.Variant("s", sid)},)))[0]
stream = call(SC, sc, SC + ".Session", "RecordMonitor", GLib.Variant("(sa{sv})", ("", {})))[0]
# Absolute pointer and touch positions map through the screen-cast stream, which only starts
# once something consumes it.
node = []
bus.signal_subscribe(SC, SC + ".Stream", "PipeWireStreamAdded", stream, None, 0,
                     lambda *a: node.append(a[5].unpack()[0]))
call(RD, session, RD + ".Session", "Start")
ctx = GLib.MainContext.default()
deadline = time.time() + 5
while not node and time.time() < deadline:
    ctx.iteration(False)
    time.sleep(0.01)
# WirePlumber won't link a stream to a stream, so consumers wait and are linked by hand.
def consume(name, *sink):
    proc = subprocess.Popen(
        ["gst-launch-1.0", "-q", "pipewiresrc", "autoconnect=false", f"client-name={name}",
         "always-copy=true", *sink],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.8)
    subprocess.run(["pw-link", "mutter:output_1", f"{name}:input_1"], capture_output=True)
    return proc


consumer = consume("cera-consumer", "!", "fakesink")
atexit.register(lambda: consumer.terminate())
time.sleep(1.0)
subprocess.run(["pw-link", "mutter:output_1", "cera-consumer:input_1"], capture_output=True)
atexit.register(consumer.terminate)
time.sleep(1.0)


def rd(method, sig=None, *args):
    call(RD, session, RD + ".Session", method, GLib.Variant(sig, args) if sig else None)


def sleep(s):
    time.sleep(s)


POS = None


def move(x, y):
    """Relative motion from the top-left corner, which works without a stream consumer."""
    global POS
    if POS is None:
        rd("NotifyPointerMotionRelative", "(dd)", -100000.0, -100000.0)
        POS = (0.0, 0.0)
        sleep(0.05)
    rd("NotifyPointerMotionRelative", "(dd)", float(x) - POS[0], float(y) - POS[1])
    POS = (float(x), float(y))


BTN = {1: 272, 2: 274, 3: 273}


def click(x, y, button=1, hold=0.05):
    move(x, y); sleep(0.05)
    rd("NotifyPointerButton", "(ib)", BTN[button], True); sleep(hold)
    rd("NotifyPointerButton", "(ib)", BTN[button], False); sleep(0.3)


def drag(points, button=1, step=0.016):
    move(*points[0]); sleep(0.05)
    rd("NotifyPointerButton", "(ib)", BTN[button], True)
    for p in points[1:]:
        move(*p); sleep(step)
    rd("NotifyPointerButton", "(ib)", BTN[button], False); sleep(0.3)


def line(x0, y0, x1, y1, n=20):
    return [(x0 + (x1 - x0) * i / n, y0 + (y1 - y0) * i / n) for i in range(n + 1)]


KEYS = {"Return": 0xff0d, "Escape": 0xff1b, "Delete": 0xffff, "BackSpace": 0xff08,
        "Control": 0xffe3, "Shift": 0xffe1, "Tab": 0xff09, "F9": 0xffc6, "Down": 0xff54}


def key(*names):
    syms = [KEYS.get(n, ord(n) if len(n) == 1 else 0) for n in names]
    for s in syms:
        rd("NotifyKeyboardKeysym", "(ub)", s, True)
    for s in reversed(syms):
        rd("NotifyKeyboardKeysym", "(ub)", s, False)
    sleep(0.2)


def type_text(text):
    for ch in text:
        key(ch)


def touch(slots_paths, step=0.016):
    """slots_paths: list of point lists, one per finger, moved together."""
    n = max(len(p) for p in slots_paths)
    for slot, p in enumerate(slots_paths):
        rd("NotifyTouchDown", "(sudd)", stream, slot, float(p[0][0]), float(p[0][1]))
    rd("NotifyTouchFrame") if False else None
    sleep(step)
    for i in range(1, n):
        for slot, p in enumerate(slots_paths):
            x, y = p[min(i, len(p) - 1)]
            rd("NotifyTouchMotion", "(sudd)", stream, slot, float(x), float(y))
        sleep(step)
    for slot in range(len(slots_paths)):
        rd("NotifyTouchUp", "(u)", slot)
    sleep(0.3)


def app(action, value=None):
    params = [value] if value is not None else []
    call("io.github.troikoss.Cera", "/io/github/troikoss/Cera", "org.gtk.Actions", "Activate",
         GLib.Variant("(sava{sv})", (action, params, {})))


def win(name, value=None):
    app("win", GLib.Variant("(sv)", (name, value if value is not None else GLib.Variant("b", False))))


def screen(name):
    """The whole screen, popovers included, from the screen-cast stream (as its only
    consumer, so the format is negotiated afresh)."""
    global consumer
    consumer.terminate(); consumer.wait()
    sleep(0.3)
    grab = consume("cera-shot", "num-buffers=2", "!", "videoconvert", "!",
                   "video/x-raw,format=RGB", "!", "pngenc", "snapshot=true", "!", "filesink",
                   f"location={OUT}/{name}.png")
    # Mutter sends frames on damage only; nudge the pointer to make some.
    for dx in (1, -1, 1, -1):
        rd("NotifyPointerMotionRelative", "(dd)", float(dx), 0.0)
        sleep(0.1)
    try:
        grab.wait(5)
    except subprocess.TimeoutExpired:
        grab.kill()
    consumer = consume("cera-consumer", "!", "fakesink")


def shot(name):
    app("screenshot", GLib.Variant("s", f"{OUT}/{name}.png"))
    sleep(0.5)


# The first key and the first touch only create mutter's virtual devices; spend them here.
rd("NotifyKeyboardKeysym", "(ub)", KEYS["Shift"], True)
rd("NotifyKeyboardKeysym", "(ub)", KEYS["Shift"], False)
sleep(0.3)

exec(sys.argv[1])
