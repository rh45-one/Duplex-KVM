"""Desktop-session capture helper.

Arguments: mode width height max_fps x_offset native_width native_height.
Descriptor 3 is the daemon's frame ring: a 4 KiB header (magic, version,
width, height, stride, slot count; per-slot sequence numbers from offset 64)
followed by the slots. Each frame is copied into the next slot while its
sequence number is odd, then stdout announces the slot as a little-endian
u32. Frames arrive when the desktop changes; above max_fps they are dropped,
never duplicated, so a still desktop costs almost nothing.
"""
import ctypes
import mmap
import os
import re
import struct
import sys
import time
import uuid
import gi

gi.require_version('Gst', '1.0')
from gi.repository import GLib, Gio, Gst

Gst.init(None)
mode, w, h, fps, x_off, native_w, native_h = sys.argv[1:8]
w, h, fps, x_off, native_w, native_h = map(int, (w, h, fps, x_off, native_w, native_h))
loop = GLib.MainLoop()
pipeline = None
ring = None
session_proxy = None
portal_session = None
remote_fd = None
failed = False


def fail(message):
    global failed
    failed = True
    print(f'Duplex capture: {message}', file=sys.stderr, flush=True)
    loop.quit()


class RateLimiter:
    """Drops frames above max_fps, keeping a steady cadence despite jitter.

    PipeWire streams have a variable rate (framerate 0/1), which videorate
    does not limit, so this works on arrival times instead.
    """

    def __init__(self, max_fps):
        self.interval = 1.0 / max_fps
        # Half an interval of slack keeps ~59 of 60 frames from a jittery
        # source while the average never exceeds max_fps.
        self.tolerance = self.interval / 2
        self.next_due = 0.0

    def __call__(self, _pad, _info):
        now = time.monotonic()
        if now + self.tolerance < self.next_due:
            return Gst.PadProbeReturn.DROP
        self.next_due = max(now, self.next_due) + self.interval
        return Gst.PadProbeReturn.OK


class MapInfo(ctypes.Structure):
    """GstMapInfo (stable GStreamer 1.x ABI)."""
    _fields_ = [('memory', ctypes.c_void_p), ('flags', ctypes.c_int), ('data', ctypes.c_void_p),
                ('size', ctypes.c_size_t), ('maxsize', ctypes.c_size_t),
                ('user_data', ctypes.c_void_p * 4), ('reserved', ctypes.c_void_p * 4)]


def load_gstreamer():
    try:
        library = ctypes.CDLL('libgstreamer-1.0.so.0')
    except OSError:
        return None
    library.gst_buffer_map.argtypes = [ctypes.c_void_p, ctypes.POINTER(MapInfo), ctypes.c_int]
    library.gst_buffer_map.restype = ctypes.c_int
    library.gst_buffer_unmap.argtypes = [ctypes.c_void_p, ctypes.POINTER(MapInfo)]
    return library


class FrameRing:
    """Writes frames into the daemon's shared slots and announces them."""

    def __init__(self):
        self.shared = mmap.mmap(3, os.fstat(3).st_size)
        magic, _version, width, height, stride, slots = struct.unpack_from('<6I', self.shared, 0)
        if magic != 0x46585044 or (width, height) != (w, h) or not slots:
            raise RuntimeError('the frame ring does not match the capture size')
        self.row, self.height, self.stride, self.slots = width * 4, height, stride, slots
        self.slot_bytes = (stride * height + 4095) // 4096 * 4096
        self.base = ctypes.addressof(ctypes.c_char.from_buffer(self.shared)) + 4096
        self.gst = load_gstreamer()
        self.next = 0
        self.warned = False

    def copy(self, buffer, target):
        size = buffer.get_size()
        if size < self.row * self.height or size % self.height:
            if not self.warned:
                self.warned = True
                print(f'Duplex capture: unexpected {size}-byte frame', file=sys.stderr, flush=True)
            return False
        source_stride = size // self.height
        # PyGObject without gst-python cannot map a buffer without copying it,
        # so map it through the C API: one memmove per frame, no allocation.
        match = re.search(r'GstBuffer at (0x[0-9a-fA-F]+)', repr(buffer))
        if self.gst and match:
            address, info = int(match.group(1), 16), MapInfo()
            if not self.gst.gst_buffer_map(address, ctypes.byref(info), 1):
                return False
            try:
                if source_stride == self.stride:
                    ctypes.memmove(target, info.data, self.stride * self.height)
                else:
                    for row in range(self.height):
                        ctypes.memmove(target + row * self.stride, info.data + row * source_stride, self.row)
            finally:
                self.gst.gst_buffer_unmap(address, ctypes.byref(info))
            return True
        data = buffer.extract_dup(0, size)
        offset = target - self.base + 4096
        for row in range(self.height):
            start = offset + row * self.stride
            self.shared[start:start + self.row] = data[row * source_stride:row * source_stride + self.row]
        return True

    def on_sample(self, sink):
        sample = sink.emit('pull-sample')
        if sample is None:
            return Gst.FlowReturn.OK
        slot, self.next = self.next, (self.next + 1) % self.slots
        sequence_at = 64 + 4 * slot
        sequence = struct.unpack_from('<I', self.shared, sequence_at)[0]
        struct.pack_into('<I', self.shared, sequence_at, (sequence + 1) & 0xFFFFFFFF)
        copied = self.copy(sample.get_buffer(), self.base + slot * self.slot_bytes)
        struct.pack_into('<I', self.shared, sequence_at, (sequence + 2) & 0xFFFFFFFF)
        if copied:
            try:
                os.write(1, struct.pack('<I', slot))
            except BrokenPipeError:
                fail('daemon closed the frame channel')
        return Gst.FlowReturn.OK


def start_pipeline(source, source_caps=''):
    global pipeline, ring
    ring = FrameRing()
    # Constraining PipeWire to the native size makes Mutter create exactly
    # that virtual monitor; videoscale is then a pass-through.
    desc = (f'{source} name=duplex_source ! '
            f'video/x-raw,width={native_w},height={native_h}{source_caps} ! '
            'videoconvert n-threads=2 ! videoscale ! '
            f'video/x-raw,format=BGRx,width={w},height={h} ! '
            'appsink name=duplex_sink emit-signals=true max-buffers=1 drop=true sync=false')
    pipeline = Gst.parse_launch(desc)
    pipeline.get_by_name('duplex_sink').connect('new-sample', ring.on_sample)
    pipewire_source = pipeline.get_by_name('duplex_source')
    pipewire_source.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, RateLimiter(fps))
    if pipewire_source.find_property('keepalive-time'):
        # The encoder refines and refreshes on its own while the desktop is
        # still; an occasional frame only proves the stream is alive.
        pipewire_source.set_property('keepalive-time', 1000)
    bus = pipeline.get_bus()
    bus.add_signal_watch()

    def on_message(_bus, msg):
        if msg.type == Gst.MessageType.ERROR:
            error, detail = msg.parse_error()
            fail(f'GStreamer: {error}; {detail}')
        elif msg.type == Gst.MessageType.EOS:
            fail('capture reached EOF')

    bus.connect('message', on_message)
    if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
        raise RuntimeError('GStreamer refused PLAYING state')


def portal_capture(bus):
    """KWin/other compositors: permission-mediated virtual source."""
    global portal_session, remote_fd
    portal = Gio.DBusProxy.new_sync(
        bus, Gio.DBusProxyFlags.NONE, None, 'org.freedesktop.portal.Desktop',
        '/org/freedesktop/portal/desktop', 'org.freedesktop.portal.ScreenCast', None)
    types = portal.get_cached_property('AvailableSourceTypes')
    if not types or not (types.unpack() & 4):
        raise RuntimeError('desktop portal does not offer virtual monitors (source type 4)')

    def request(method, signature, args):
        token = 'duplex_' + uuid.uuid4().hex
        args[-1]['handle_token'] = GLib.Variant('s', token)
        sender = bus.get_unique_name()[1:].replace('.', '_')
        path = f'/org/freedesktop/portal/desktop/request/{sender}/{token}'
        result = []
        wait = GLib.MainLoop()

        def response(_conn, _sender, _path, _iface, _sig, params):
            result.append(params.unpack())
            wait.quit()

        subscription = bus.signal_subscribe(
            'org.freedesktop.portal.Desktop', 'org.freedesktop.portal.Request',
            'Response', path, None, Gio.DBusSignalFlags.NONE, response)
        timeout = GLib.timeout_add_seconds(120, lambda: (wait.quit(), False)[1])
        try:
            portal.call_sync(method, GLib.Variant(signature, args), Gio.DBusCallFlags.NONE, 5000, None)
            if not result:
                wait.run()
            if not result or result[0][0] != 0:
                raise RuntimeError(f'portal {method} cancelled, denied, or timed out')
            return result[0][1]
        finally:
            bus.signal_unsubscribe(subscription)
            # A fired timeout removes itself.
            if result:
                GLib.source_remove(timeout)

    portal_session = request('CreateSession', '(a{sv})', ({
        'session_handle_token': GLib.Variant('s', 'duplex_' + uuid.uuid4().hex)},))['session_handle']
    bus.signal_subscribe('org.freedesktop.portal.Desktop', 'org.freedesktop.portal.Session',
                         'Closed', portal_session, None, Gio.DBusSignalFlags.NONE,
                         lambda *args: fail('portal session closed'))
    request('SelectSources', '(oa{sv})', (portal_session, {
        'types': GLib.Variant('u', 4), 'multiple': GLib.Variant('b', False)}))
    streams = request('Start', '(osa{sv})', (portal_session, '', {}))['streams']
    if len(streams) != 1:
        raise RuntimeError('expected one virtual stream per capture session')
    reply, fds = portal.call_with_unix_fd_list_sync(
        'OpenPipeWireRemote', GLib.Variant('(oa{sv})', (portal_session, {})),
        Gio.DBusCallFlags.NONE, 5000, None, None)
    remote_fd = fds.get(reply.unpack()[0])
    start_pipeline(f'pipewiresrc fd={remote_fd} path={streams[0][0]} do-timestamp=true')


try:
    if mode == 'test':  # Explicit integration test only.
        start_pipeline('videotestsrc is-live=true pattern=ball')
    elif (os.environ.get('XDG_SESSION_TYPE') == 'x11' or
          (os.environ.get('DISPLAY') and not os.environ.get('WAYLAND_DISPLAY') and
           os.environ.get('XDG_SESSION_TYPE') != 'wayland')):
        # X11 has no change notification here: poll at no more than 30 fps.
        start_pipeline(f'ximagesrc use-damage=0 show-pointer=true startx={x_off} starty=0 '
                       f'endx={x_off + native_w - 1} endy={native_h - 1}',
                       f',framerate={min(fps, 30)}/1')
    else:
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        has_mutter = bus.call_sync(
            'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
            'NameHasOwner', GLib.Variant('(s)', ('org.gnome.Mutter.ScreenCast',)),
            GLib.VariantType.new('(b)'), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
        if not has_mutter:
            portal_capture(bus)
        else:
            sc = Gio.DBusProxy.new_sync(
                bus, Gio.DBusProxyFlags.NONE, None, 'org.gnome.Mutter.ScreenCast',
                '/org/gnome/Mutter/ScreenCast', 'org.gnome.Mutter.ScreenCast', None)
            path = sc.call_sync('CreateSession', GLib.Variant('(a{sv})', ({},)),
                                Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
            session_proxy = Gio.DBusProxy.new_sync(
                bus, Gio.DBusProxyFlags.NONE, None, 'org.gnome.Mutter.ScreenCast',
                path, 'org.gnome.Mutter.ScreenCast.Session', None)
            stream_path = session_proxy.call_sync(
                'RecordVirtual', GLib.Variant('(a{sv})', ({
                    'cursor-mode': GLib.Variant('u', 1)},)),
                Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]

            def on_stream(_conn, _sender, _path, _iface, _sig, params):
                try:
                    # The virtual monitor refreshes at the negotiated maximum,
                    # and Mutter's frame limiter skips every other frame when
                    # that equals the content rate. Asking for twice the
                    # target delivers the full target rate (e.g. 60 fps).
                    start_pipeline(f'pipewiresrc path={params.unpack()[0]} do-timestamp=true',
                                   f',max-framerate={min(2 * fps, 240)}/1')
                except Exception as error:
                    fail(error)

            bus.signal_subscribe('org.gnome.Mutter.ScreenCast', 'org.gnome.Mutter.ScreenCast.Stream',
                                 'PipeWireStreamAdded', stream_path, None,
                                 Gio.DBusSignalFlags.NONE, on_stream)
            bus.signal_subscribe('org.gnome.Mutter.ScreenCast', 'org.gnome.Mutter.ScreenCast.Session',
                                 'Closed', path, None, Gio.DBusSignalFlags.NONE,
                                 lambda *args: fail('Mutter session closed'))
            session_proxy.call_sync('Start', None, Gio.DBusCallFlags.NONE, 5000, None)
    if not failed:
        loop.run()
except Exception as error:
    fail(error)
finally:
    if pipeline:
        pipeline.set_state(Gst.State.NULL)
    if session_proxy:
        try:
            session_proxy.call_sync('Stop', None, Gio.DBusCallFlags.NONE, 1000, None)
        except Exception:
            pass
    if portal_session:
        try:
            bus.call_sync('org.freedesktop.portal.Desktop', portal_session,
                          'org.freedesktop.portal.Session', 'Close', None, None,
                          Gio.DBusCallFlags.NONE, 1000, None)
        except Exception:
            pass
    if remote_fd is not None:
        os.close(remote_fd)
sys.exit(1 if failed else 0)
