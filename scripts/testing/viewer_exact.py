"""Checks that the Linux display viewer is pixel-exact at a desktop scale.

Run inside the private headless GNOME Shell:

    dbus-run-session -- scripts/testing/headless-gnome.sh \
        python3 scripts/testing/viewer_exact.py 1.25

Sets the virtual monitor's scale, feeds a one-pixel checkerboard through the
daemon's shared-memory protocol, captures the compositor output with Mutter
ScreenCast and compares every pixel. Exits non-zero on any difference.
"""
import mmap
import os
import struct
import subprocess
import sys
import time

import gi
gi.require_version('Gst', '1.0')
from gi.repository import GLib, Gio, Gst

HERE = os.path.dirname(os.path.abspath(__file__))
SCALE = float(sys.argv[1]) if len(sys.argv) > 1 else 1.25
VIEWER = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    HERE, '..', '..', 'crates', 'duplex-pal', 'src', 'linux', 'viewer.py')
Gst.init(None)
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)


def call(name, path, interface, method, args=None):
    return bus.call_sync(name, path, interface, method, args, None,
                         Gio.DBusCallFlags.NONE, 5000, None).unpack()


def display_state():
    return call('org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
                'org.gnome.Mutter.DisplayConfig', 'GetCurrentState')


serial, monitors, _logical, _ = display_state()
connector = monitors[0][0][0]
mode_id, width, height = next((m[0], m[1], m[2]) for m in monitors[0][1] if m[-1].get('is-current'))
call('org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
     'org.gnome.Mutter.DisplayConfig', 'ApplyMonitorsConfig',
     GLib.Variant('(uua(iiduba(ssa{sv}))a{sv})',
                  (serial, 1, [(0, 0, SCALE, 0, True, [(connector, mode_id, {})])], {})))
time.sleep(1)

# One-pixel checkerboard (any resampling blurs it) with a red origin marker.
pattern = bytearray(width * height * 4)
for y in range(height):
    for x in range(width):
        if x < 64 and y < 64:
            colour = (0, 0, 255)
        else:
            colour = (255, 255, 255) if (x + y) % 2 == 0 else (0, 0, 0)
        i = (y * width + x) * 4
        pattern[i:i + 4] = bytes((*colour, 255))
fd = os.memfd_create('duplex-viewer-test')
size = 4096 + width * height * 4
os.ftruncate(fd, size)
shared = mmap.mmap(fd, size)
struct.pack_into('<6I', shared, 0, 0x53585044, 1, width, height, width * 4, 0)
shared[4096:4096 + len(pattern)] = pattern
os.dup2(fd, 3, inheritable=True)
with open(VIEWER) as source:
    viewer = subprocess.Popen([sys.executable, '-c', source.read(), '0'],
                              stdin=subprocess.PIPE, pass_fds=(3,))
viewer.stdin.write(struct.pack('<5I', 1, 0, 0, width, height))
viewer.stdin.flush()
time.sleep(3)

screencast = Gio.DBusProxy.new_sync(bus, Gio.DBusProxyFlags.NONE, None, 'org.gnome.Mutter.ScreenCast',
                                    '/org/gnome/Mutter/ScreenCast', 'org.gnome.Mutter.ScreenCast', None)
session_path = screencast.call_sync('CreateSession', GLib.Variant('(a{sv})', ({},)),
                                    Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
session = Gio.DBusProxy.new_sync(bus, Gio.DBusProxyFlags.NONE, None, 'org.gnome.Mutter.ScreenCast',
                                 session_path, 'org.gnome.Mutter.ScreenCast.Session', None)
stream_path = session.call_sync(
    'RecordMonitor', GLib.Variant('(sa{sv})', (connector, {'cursor-mode': GLib.Variant('u', 0)})),
    Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
loop = GLib.MainLoop()
frame = {}


def on_stream(*args):
    node = args[-1].unpack()[0]
    pipeline = Gst.parse_launch(f'pipewiresrc path={node} num-buffers=5 ! videoconvert ! '
                                'video/x-raw,format=BGRx ! appsink name=sink emit-signals=true sync=false')

    def on_sample(sink):
        sample = sink.emit('pull-sample')
        caps = sample.get_caps().get_structure(0)
        buffer = sample.get_buffer()
        frame['size'] = (caps.get_value('width'), caps.get_value('height'))
        frame['data'] = buffer.extract_dup(0, buffer.get_size())
        return Gst.FlowReturn.OK

    pipeline.get_by_name('sink').connect('new-sample', on_sample)
    pipeline.set_state(Gst.State.PLAYING)
    frame['pipeline'] = pipeline
    GLib.timeout_add(1500, loop.quit)


bus.signal_subscribe('org.gnome.Mutter.ScreenCast', 'org.gnome.Mutter.ScreenCast.Stream',
                     'PipeWireStreamAdded', stream_path, None, Gio.DBusSignalFlags.NONE, on_stream)
session.call_sync('Start', None, Gio.DBusCallFlags.NONE, 5000, None)
GLib.timeout_add(10000, loop.quit)
loop.run()
if 'pipeline' in frame:
    frame['pipeline'].set_state(Gst.State.NULL)
viewer.kill()

if 'data' not in frame:
    sys.exit('no frame captured')
captured_width, captured_height = frame['size']
if (captured_width, captured_height) != (width, height):
    sys.exit(f'captured {captured_width}x{captured_height}, monitor is {width}x{height}')
data = frame['data']
stride = len(data) // height
mismatches = sum(
    data[y * stride + x * 4:y * stride + x * 4 + 3] != pattern[(y * width + x) * 4:(y * width + x) * 4 + 3]
    for y in range(height) for x in range(width))
print(f'scale {SCALE}: {mismatches} of {width * height} pixels differ')
sys.exit(1 if mismatches else 0)
