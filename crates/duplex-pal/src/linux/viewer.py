"""Duplex-KVM fullscreen display viewer.

Descriptor 3 is the daemon's shared canvas: a 4 KiB header (magic, version,
width, height, stride, sequence) followed by BGRx rows. The sequence is odd
while the daemon writes, so a copy taken between two equal even values is a
complete frame. Standard input announces changed regions as
``count`` + ``count`` * (x, y, width, height), little-endian u32.

GTK 4 draws the frame at exact device pixels, including fractional desktop
scaling, so text stays sharp. GTK 3 is a fallback for older systems.
"""
import mmap
import os
import struct
import sys

import gi

MONITOR = int(sys.argv[1]) if len(sys.argv) > 1 else 0
TITLE = f'Duplex — remote display {MONITOR + 1}'
HEADER = 4096
MAGIC = 0x53585044  # b'DPXS'
shared = mmap.mmap(3, os.fstat(3).st_size, mmap.MAP_SHARED, mmap.PROT_READ)
os.set_blocking(0, False)


def snapshot():
    """Returns (width, height, stride, pixels) of a consistent frame, or None."""
    result = None
    for _ in range(64):
        before = struct.unpack_from('<I', shared, 20)[0]
        if before & 1:
            os.sched_yield()
            continue
        magic, _version, width, height, stride = struct.unpack_from('<5I', shared, 0)
        if magic != MAGIC or not width or not height or stride < width * 4:
            return None
        result = (width, height, stride, shared[HEADER:HEADER + stride * height])
        if struct.unpack_from('<I', shared, 20)[0] == before:
            return result
    return result


class Notifications:
    """Parses region announcements from standard input."""

    def __init__(self, on_change, on_close):
        self.buffer = b''
        self.on_change = on_change
        self.on_close = on_close

    def __call__(self, *_):
        try:
            chunk = os.read(0, 65536)
        except BlockingIOError:
            return True
        if not chunk:
            self.on_close()
            return False
        self.buffer += chunk
        rects = []
        while len(self.buffer) >= 4:
            count = struct.unpack_from('<I', self.buffer)[0]
            size = 4 + 16 * count
            if count > 4096:
                print('Duplex viewer: corrupt notification stream', file=sys.stderr)
                self.on_close()
                return False
            if len(self.buffer) < size:
                break
            rects.extend(struct.iter_unpack('<4I', self.buffer[4:size]))
            self.buffer = self.buffer[size:]
        if rects:
            self.on_change(rects)
        return True


def clamp(rects, width, height):
    for x, y, w, h in rects:
        x, y = min(x, width), min(y, height)
        w, h = min(w, width - x), min(h, height - y)
        if w and h:
            yield x, y, w, h


def run_gtk4():
    gi.require_version('Gtk', '4.0')
    gi.require_version('Gdk', '4.0')
    gi.require_version('Gsk', '4.0')
    gi.require_version('Graphene', '1.0')
    from gi.repository import GLib, Gdk, Graphene, Gsk, Gtk
    gi.require_foreign('cairo')
    import cairo

    if not Gtk.init_check():
        sys.exit('Duplex viewer: cannot open the desktop display')
    builder_supported = hasattr(Gdk, 'MemoryTextureBuilder')
    memory_format = getattr(Gdk.MemoryFormat, 'B8G8R8X8', Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED)
    black = Gdk.RGBA()
    black.parse('black')

    class Canvas(Gtk.Widget):
        texture = None

        def device_origin(self, scale):
            """Device-pixel position of this widget within its surface."""
            native = self.get_native()
            if native is None:
                return 0.0, 0.0
            x = y = 0.0
            if hasattr(self, 'compute_point'):
                ok, point = self.compute_point(native, Graphene.Point().init(0, 0))
                if ok:
                    x, y = point.x, point.y
            if hasattr(native, 'get_surface_transform'):
                sx, sy = native.get_surface_transform()
                x, y = x + sx, y + sy
            return x * scale, y * scale

        def do_snapshot(self, snap):
            width, height = self.get_width(), self.get_height()
            snap.append_color(black, Graphene.Rect().init(0, 0, width, height))
            texture = self.texture
            if texture is None or width <= 0 or height <= 0:
                return
            native = self.get_native()
            surface = native.get_surface() if native else None
            if surface is not None and hasattr(surface, 'get_scale'):
                scale = surface.get_scale()
            else:
                scale = float(self.get_scale_factor())
            tw, th = texture.get_width(), texture.get_height()
            fit = min(width * scale / tw, height * scale / th)
            if abs(fit - 1.0) < 0.002:
                fit = 1.0
            out_w, out_h = tw * fit, th * fit
            # Align the image's corner to the device pixel grid: at a 1:1 fit
            # every source pixel then lands on exactly one screen pixel.
            ox, oy = self.device_origin(scale)
            left = round(ox + (width * scale - out_w) / 2) - ox
            top = round(oy + (height * scale - out_h) / 2) - oy
            rect = Graphene.Rect().init(left / scale, top / scale, out_w / scale, out_h / scale)
            if fit == 1.0 or not hasattr(snap, 'append_scaled_texture'):
                # A plain texture node at exactly its device size is sampled
                # 1:1. (Scaled-texture nodes may render through an offscreen
                # at logical resolution under fractional scaling, which blurs.)
                snap.append_texture(texture, rect)
            else:
                quality = Gsk.ScalingFilter.TRILINEAR if fit < 1.0 else Gsk.ScalingFilter.LINEAR
                snap.append_scaled_texture(texture, quality, rect)

    loop = GLib.MainLoop()
    window = Gtk.Window(title=TITLE)
    window.set_default_size(1280, 800)
    canvas = Canvas()
    window.set_child(canvas)
    window.set_cursor(Gdk.Cursor.new_from_name('none', None))
    css = Gtk.CssProvider()
    css.load_from_data(b'window { background: black; }', -1)
    Gtk.StyleContext.add_provider_for_display(
        Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)

    def on_key(_controller, keyval, _code, _state):
        if keyval == Gdk.KEY_F11:
            if window.is_fullscreen():
                window.unfullscreen()
            else:
                window.fullscreen()
            return True
        return False

    keys = Gtk.EventControllerKey()
    keys.connect('key-pressed', on_key)
    window.add_controller(keys)
    window.connect('close-request', lambda *_: (loop.quit(), False)[1])

    pending = {'rects': [], 'scheduled': False}

    def update():
        pending['scheduled'] = False
        rects, pending['rects'] = pending['rects'], []
        frame = snapshot()
        if frame is None:
            return False
        width, height, stride, pixels = frame
        data = GLib.Bytes.new(pixels)
        previous = canvas.texture
        if builder_supported:
            builder = Gdk.MemoryTextureBuilder()
            builder.set_bytes(data)
            builder.set_width(width)
            builder.set_height(height)
            builder.set_stride(stride)
            builder.set_format(memory_format)
            changed = list(clamp(rects, width, height))
            if previous is not None and changed and \
                    (previous.get_width(), previous.get_height()) == (width, height):
                # Only the changed regions are uploaded to the GPU again.
                builder.set_update_texture(previous)
                builder.set_update_region(cairo.Region([cairo.RectangleInt(*r) for r in changed]))
            texture = builder.build()
        else:
            texture = Gdk.MemoryTexture.new(width, height, memory_format, data, stride)
        canvas.texture = texture
        canvas.queue_draw()
        return False

    def changed(rects):
        pending['rects'].extend(rects)
        if not pending['scheduled']:
            pending['scheduled'] = True
            GLib.idle_add(update)

    GLib.unix_fd_add_full(GLib.PRIORITY_DEFAULT, 0,
                          GLib.IOCondition.IN | GLib.IOCondition.HUP | GLib.IOCondition.ERR,
                          Notifications(changed, loop.quit))
    window.fullscreen()
    window.present()
    changed([])
    loop.run()


def run_gtk3():
    gi.require_version('Gtk', '3.0')
    gi.require_version('Gdk', '3.0')
    from gi.repository import GLib, Gdk, Gtk
    import cairo

    if not Gtk.init_check(sys.argv)[0]:
        sys.exit('Duplex viewer: cannot open the desktop display')
    state = {'image': None, 'scheduled': False}
    window = Gtk.Window(title=TITLE)
    window.set_default_size(1280, 800)
    area = Gtk.DrawingArea()
    window.add(area)
    window.connect('destroy', lambda *_: Gtk.main_quit())

    def draw(_widget, cr):
        allocation = area.get_allocation()
        cr.set_source_rgb(0, 0, 0)
        cr.paint()
        image = state['image']
        if image is None:
            return False
        surface = image[0]
        tw, th = surface.get_width(), surface.get_height()
        fit = min(allocation.width / tw, allocation.height / th)
        cr.translate(round((allocation.width - tw * fit) / 2), round((allocation.height - th * fit) / 2))
        cr.scale(fit, fit)
        cr.set_source_surface(surface, 0, 0)
        cr.get_source().set_filter(cairo.FILTER_GOOD if abs(fit - 1) > 0.002 else cairo.FILTER_NEAREST)
        cr.paint()
        return False

    def update():
        state['scheduled'] = False
        frame = snapshot()
        if frame is not None:
            width, height, stride, pixels = frame
            buffer = bytearray(pixels)
            surface = cairo.ImageSurface.create_for_data(buffer, cairo.FORMAT_RGB24, width, height, stride)
            state['image'] = (surface, buffer)
            area.queue_draw()
        return False

    def changed(_rects):
        if not state['scheduled']:
            state['scheduled'] = True
            GLib.idle_add(update)

    def on_key(_widget, event):
        if event.keyval == Gdk.KEY_F11:
            state_flags = window.get_window().get_state() if window.get_window() else 0
            if state_flags & Gdk.WindowState.FULLSCREEN:
                window.unfullscreen()
            else:
                window.fullscreen()
            return True
        return False

    def on_realize(_widget):
        display = window.get_display()
        window.get_window().set_cursor(Gdk.Cursor.new_for_display(display, Gdk.CursorType.BLANK_CURSOR))

    area.connect('draw', draw)
    window.connect('key-press-event', on_key)
    window.connect('realize', on_realize)
    GLib.unix_fd_add_full(GLib.PRIORITY_DEFAULT, 0,
                          GLib.IOCondition.IN | GLib.IOCondition.HUP | GLib.IOCondition.ERR,
                          Notifications(changed, Gtk.main_quit))
    window.fullscreen()
    window.show_all()
    changed([])
    Gtk.main()


def gtk4_available():
    try:
        gi.require_version('Gtk', '4.0')
        gi.require_version('Gsk', '4.0')
        gi.require_version('Graphene', '1.0')
        return True
    except ValueError:
        return False


if os.environ.get('DUPLEX_VIEWER_GTK') != '3' and gtk4_available():
    run_gtk4()
else:
    print('Duplex viewer: using GTK 3 (install gir1.2-gtk-4.0 for pixel-exact '
          'output with fractional scaling)', file=sys.stderr)
    run_gtk3()
