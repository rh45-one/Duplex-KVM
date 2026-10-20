"""Temporarily arrange only the virtual outputs created by this capture run."""
import json
import sys
import time


def current_mode(monitor):
    return next((mode for mode in monitor[1] if mode[-1].get('is-current')), None)


def plan_layout(state, baseline, expected):
    _serial, monitors, logical, properties = state
    by_connector = {monitor[0][0]: monitor for monitor in monitors}
    owned = [monitor for monitor in monitors
             if monitor[0][0] not in baseline and monitor[0][1] == 'MetaVendor'
             and monitor[0][2] == 'Virtual remote monitor']
    if len(owned) != expected or any(current_mode(monitor) is None for monitor in owned):
        return None
    owned.sort(key=lambda monitor: monitor[0][0])
    names = {monitor[0][0] for monitor in owned}
    retained = [group for group in logical if not any(item[0] in names for item in group[5])]
    if not retained:
        raise RuntimeError('Refusing to change a layout without the physical desktop')
    anchor = next((group for group in retained if group[4]), retained[0])

    def dimensions(group):
        mode = current_mode(by_connector[group[5][0][0]])
        width, height = mode[1:3]
        if group[3] % 2:
            width, height = height, width
        scale = group[2] if properties.get('layout-mode', 1) == 1 else 1
        return round(width / scale), round(height / scale)

    anchor_height = dimensions(anchor)[1]
    candidates = []
    for monitor in owned:
        mode = current_mode(monitor)
        # The logical height never exceeds the laptop's edge. Prefer a shared
        # height so returning through either remote monitor cannot hit a gap.
        options = {round(mode[2] / scale): scale for scale in mode[5]
                   if round(mode[2] / scale) <= anchor_height}
        if not options:
            raise RuntimeError('No supported scale fits the laptop edge')
        candidates.append(options)
    common = set(candidates[0])
    for options in candidates[1:]:
        common.intersection_update(options)
    shared_height = max(common) if common else None
    x = max(group[0] + dimensions(group)[0] for group in retained)
    y = anchor[1]
    config = []
    for group in retained:
        targets = [(item[0], current_mode(by_connector[item[0]])[0], {}) for item in group[5]]
        config.append((group[0], group[1], group[2], group[3], group[4], targets))
    for monitor, options in zip(owned, candidates):
        mode = current_mode(monitor)
        height = shared_height if shared_height is not None else max(options)
        scale = options[height]
        config.append((x, y, scale, 0, False, [(monitor[0][0], mode[0], {})]))
        x += round(mode[1] / scale)
    return config


def main():
    from gi.repository import Gio, GLib
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)

    def state():
        return bus.call_sync('org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
                             'org.gnome.Mutter.DisplayConfig', 'GetCurrentState', None, None,
                             Gio.DBusCallFlags.NONE, 2000, None).unpack()

    if sys.argv[1] == 'snapshot':
        print(json.dumps([monitor[0][0] for monitor in state()[1]]))
        return
    baseline = set(json.loads(sys.argv[2]))
    expected = int(sys.argv[3])
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        current = state()
        config = plan_layout(current, baseline, expected)
        if config:
            args = GLib.Variant('(uua(iiduba(ssa{sv}))a{sv})',
                                (current[0], 1, config, {}))
            bus.call_sync('org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
                          'org.gnome.Mutter.DisplayConfig', 'ApplyMonitorsConfig', args, None,
                          Gio.DBusCallFlags.NONE, 3000, None)
            print('Duplex layout: temporary contiguous virtual outputs applied: ' +
                  repr([(entry[0], entry[1], entry[2]) for entry in config]), file=sys.stderr)
            return
        time.sleep(0.2)
    raise RuntimeError('Virtual outputs did not become available for layout configuration')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Duplex layout: {error}', file=sys.stderr)
        sys.exit(1)
