import importlib.util
from pathlib import Path
import unittest

source = Path(__file__).resolve().parents[1] / 'crates/duplex-pal/src/linux/layout.py'
spec = importlib.util.spec_from_file_location('duplex_layout', source)
layout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layout)


def monitor(name, width, height, scales, physical=False):
    identity = (name, 'BOE' if physical else 'MetaVendor',
                'Panel' if physical else 'Virtual remote monitor', name)
    mode = ('native', width, height, 60., 1., scales, {'is-current': True})
    return (identity, [mode], {})


class LayoutTests(unittest.TestCase):
    def test_scaled_laptop_and_mixed_resolution_monitors_have_return_path(self):
        panel = monitor('eDP-1', 1920, 1200, [1., 1.25], True)
        main = monitor('Meta-0', 3840, 2160, [1., 1.25, 1.5, 2., 2.5, 3.])
        second = monitor('Meta-1', 1920, 1080, [1., 1.25, 1.5, 2.])
        groups = [(0, 0, 1.25, 0, True, [panel[0]], {}),
                  (1536, 0, 1., 0, False, [main[0]], {}),
                  (5376, 0, 1., 0, False, [second[0]], {})]
        plan = layout.plan_layout((1, [panel, main, second], groups, {'layout-mode': 1}), ['eDP-1'], 2)
        self.assertEqual([group[:3] for group in plan], [(0, 0, 1.25), (1536, 0, 2.5), (3072, 0, 1.25)])
        self.assertEqual(2160 / plan[1][2], 1080 / plan[2][2])
        self.assertLessEqual(2160 / plan[1][2], 1200 / plan[0][2])
        self.assertEqual(plan[0][4], True)

    def test_waits_for_both_outputs_and_ignores_existing_virtual_monitor(self):
        panel = monitor('eDP-1', 1920, 1200, [1., 1.25], True)
        virtual = monitor('Meta-0', 1920, 1080, [1.])
        state = (1, [panel, virtual], [(0, 0, 1.25, 0, True, [panel[0]], {})], {})
        self.assertIsNone(layout.plan_layout(state, ['eDP-1'], 2))
        self.assertIsNone(layout.plan_layout(state, ['eDP-1', 'Meta-0'], 1))


if __name__ == '__main__':
    unittest.main()
