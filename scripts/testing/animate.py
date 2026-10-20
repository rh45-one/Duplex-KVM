"""Fullscreen animated load for capture tests: animate.py MONITOR MODE.

Modes: 'photo' pans a photo (every tile changes, photographic), 'scroll'
scrolls text (every tile changes, lossless), 'typing' adds a few characters
per frame to static text, 'flat' changes a solid colour.
"""
import os
import sys

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
gi.require_version('Graphene', '1.0')
from gi.repository import GLib, Gdk, Graphene, Gtk

monitor_index = int(sys.argv[1]) if len(sys.argv) > 1 else 0
mode = sys.argv[2] if len(sys.argv) > 2 else 'photo'
Gtk.init()
PHOTO = '/usr/share/backgrounds/gnome/adwaita-d.jpg'
photo = Gdk.Texture.new_from_filename(PHOTO) if mode == 'photo' else None
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..',
                       'crates', 'duplexd', 'src', 'lib.rs')) as source:
    lines = source.read().splitlines()


def rgba(spec):
    colour = Gdk.RGBA()
    colour.parse(spec)
    return colour


class Scene(Gtk.Widget):
    frame = 0

    def do_snapshot(self, snap):
        w, h = self.get_width(), self.get_height()
        self.frame += 1
        if mode == 'flat':
            colour = Gdk.RGBA()
            colour.red, colour.green, colour.blue, colour.alpha = (self.frame % 256) / 255, 0.2, 0.4, 1
            snap.append_color(colour, Graphene.Rect().init(0, 0, w, h))
            return
        if mode == 'photo':
            shift = (self.frame * 7) % 400
            snap.append_texture(photo, Graphene.Rect().init(-shift, -shift / 2, w + 400, h + 400))
            return
        snap.append_color(rgba('#1e1e22'), Graphene.Rect().init(0, 0, w, h))
        offset = self.frame * 3 if mode == 'scroll' else 0
        text = '\n'.join(lines[(offset // 20 + i) % len(lines)] for i in range(h // 18 + 2))
        if mode == 'typing':
            text += '\n' + 'x' * (self.frame % 80)
        layout = self.create_pango_layout(text)
        snap.save()
        point = Graphene.Point()
        point.x, point.y = 8, -(offset % 20)
        snap.translate(point)
        snap.append_layout(layout, rgba('#e0e0e6'))
        snap.restore()


window = Gtk.Window()
scene = Scene()
window.set_child(scene)
scene.add_tick_callback(lambda widget, _clock: (widget.queue_draw(), True)[1])
monitors = Gdk.Display.get_default().get_monitors()
window.fullscreen_on_monitor(monitors.get_item(min(monitor_index, monitors.get_n_items() - 1)))
window.present()
GLib.MainLoop().run()
