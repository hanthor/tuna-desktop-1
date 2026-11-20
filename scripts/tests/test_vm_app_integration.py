"""V-SWITCH-MONITOR gate logic on a simulated guest; no VM or QEMU."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'lib'))
import vm_app_integration as app  # noqa: E402

FIRST = {'connector': 'Virtual-1', 'builtin': False, 'preferred': [1280, 800], 'current': [1280, 800],
         'modes': [[1280, 800], [1920, 1080], [1024, 768], [800, 600]]}
SECOND = {'connector': 'Virtual-2', 'builtin': False, 'preferred': [1024, 768], 'current': [1024, 768],
          'modes': [[1024, 768], [1920, 1080], [1280, 1024], [800, 600]]}


def logical(x, y, monitors, primary=False, scale=1.0, transform=0):
    return {'x': x, 'y': y, 'scale': scale, 'transform': transform, 'primary': primary, 'monitors': monitors}


def joined():
    return {'monitors': [dict(FIRST), dict(SECOND)],
            'logical': [logical(0, 0, ['Virtual-1'], True), logical(1280, 0, ['Virtual-2'])]}


def mirrored():
    return {'monitors': [dict(FIRST, current=[1920, 1080]), dict(SECOND, current=[1920, 1080])],
            'logical': [logical(0, 0, ['Virtual-1', 'Virtual-2'], True)]}


class Clock:
    """Stands in for the module's time: sleeping advances it at once."""
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds

    def time(self):
        return 1_700_000_000 + self.now


class Guest:
    """A two-head guest whose shell follows switchMonitor.js.

    QMP key events drive it like the real popup: the binding with Super held
    opens it on the choice after the one in effect, each further press
    steps, and releasing Super applies the selection through mutter's
    switch config. The flags drop one piece each, as an older or broken
    build would."""
    def __init__(self, opens=True, applies=True, confirms=False, closes_on_step=False):
        self.opens, self.applies, self.confirms, self.closes_on_step = opens, applies, confirms, closes_on_step
        self.held, self.events, self.frames = set(), [], []
        self.selected = None
        self.current = None
        self.display = {'monitors': [dict(FIRST)], 'logical': [logical(0, 0, ['Virtual-1'], True)]}

    # QEMU's side
    def plug(self, connected):
        self.display = joined() if connected else {
            'monitors': [dict(FIRST)], 'logical': [logical(0, 0, ['Virtual-1'], True)]}
        self.current = None

    def cmd(self, command, **arguments):
        assert command == 'input-send-event', command
        (event,) = arguments['events']
        name, down = event['data']['key']['data'], event['data']['down']
        self.events.append((name, down))
        if down:
            self.held.add(name)
        else:
            self.held.discard(name)
        if name == 'p' and down and 'meta_l' in self.held and len(self.display['monitors']) > 1 and self.opens:
            if self.selected is None:
                self.selected = ((app.SWITCH_CHOICES.index(self.current) if self.current else 4) + 1) % 2
            elif self.closes_on_step:
                self.selected = None
            else:
                self.selected = (self.selected + 1) % 2
        if name == 'meta_l' and not down and self.selected is not None:
            choice, self.selected = app.SWITCH_CHOICES[self.selected], None
            if self.applies:
                self.current = choice
                self.display = mirrored() if choice == 'Mirror' else joined()

    def frame(self, path):
        self.frames.append(Path(path).name)

    # the guest agent's side
    def run(self, action, *args, timeout=60):
        if action == 'app-scene':
            layers = [{'namespace': 'tuna-panel', 'rect': [0, 0, 1280, 32]}]
            if self.selected is not None:
                layers.append({'namespace': app.SWITCH_POPUP, 'rect': [440, 300, 400, 200]})
            return {'layers': layers, 'display_config': {'awaiting_confirmation': self.confirms}}
        if action == 'app-display':
            return copy.deepcopy(self.display)
        raise AssertionError(action)

    def close(self):
        pass


class Run(unittest.TestCase):
    def switch(self, guest):
        clock = Clock()
        with tempfile.TemporaryDirectory() as out, \
                mock.patch.object(app, 'time', clock), \
                mock.patch.object(app.Lane, 'set_head',
                                  lambda lane, head, width, height, x=0, y=0: guest.plug(bool(width))):
            lane = app.Lane(guest, lambda: guest, out, None, 'unix:path=/unused')
            failures = lane.run_switch_monitor()
            lane.close()
            evidence = json.loads((Path(out) / 'app-integration/switch-monitor.json').read_text())
        return failures, evidence

    def test_gnome_popup_mirrors_then_joins(self):
        guest = Guest()
        failures, evidence = self.switch(guest)
        self.assertEqual(failures, [])
        # Mirror: Super held, P opens on Join Displays, P again, release;
        # then Join Displays is the choice after Mirror: one press.
        self.assertEqual(guest.events, [
            ('meta_l', True), ('p', True), ('p', False), ('p', True), ('p', False), ('meta_l', False),
            ('meta_l', True), ('p', True), ('p', False), ('meta_l', False)])
        self.assertEqual([s['target'] for s in evidence['steps']], ['Mirror', 'Join Displays'])
        self.assertEqual([s['presses'] for s in evidence['steps']], [2, 1])
        self.assertEqual(evidence['steps'][0]['applied'], mirrored())
        self.assertEqual(evidence['steps'][1]['applied'], joined())
        self.assertEqual(evidence['steps'][0]['popup'][0]['namespace'], app.SWITCH_POPUP)
        self.assertEqual(len(evidence['unplugged']['monitors']), 1)
        self.assertIn('switch-monitor-mirror-popup.png', guest.frames)

    def test_a_shell_without_the_popup_fails(self):
        guest = Guest(opens=False)
        failures, evidence = self.switch(guest)
        self.assertEqual(len(failures), 1)
        self.assertIn('Super+P did not open the switch-monitor popup for Mirror', failures[0])
        self.assertEqual(guest.events[-1], ('meta_l', False))
        self.assertNotIn('meta_l', guest.held)
        self.assertEqual(len(evidence['unplugged']['monitors']), 1)

    def test_a_choice_that_changes_nothing_fails(self):
        failures, evidence = self.switch(Guest(applies=False))
        self.assertEqual(len(failures), 1)
        self.assertIn('Mirror never applied', failures[0])
        self.assertIn('2 logical monitors', failures[0])
        self.assertEqual(evidence['steps'][0]['failure']['display'], joined())

    def test_a_choice_asking_to_keep_the_changes_fails(self):
        failures, _ = self.switch(Guest(confirms=True))
        self.assertEqual(failures, ['Mirror asks to keep the changes; the popup applies a temporary configuration'])

    def test_a_popup_gone_before_super_is_released_fails(self):
        failures, _ = self.switch(Guest(closes_on_step=True))
        self.assertEqual(failures, ['the switch-monitor popup closed before Super was released (Mirror)'])

    def test_run_records_every_gate_once(self):
        recorded = []
        guest = Guest()
        with tempfile.TemporaryDirectory() as out, \
                mock.patch.object(app.Lane, 'app', lambda lane, action, *args, **kwargs: {}), \
                mock.patch.object(app.Lane, 'run_picker', lambda lane, role: []), \
                mock.patch.object(app.Lane, 'run_screen_size', lambda lane: []), \
                mock.patch.object(app.Lane, 'run_sandbox', lambda lane: []), \
                mock.patch.object(app.Lane, 'run_switch_monitor', lambda lane: ['Mirror never applied']):
            app.run(guest, lambda: guest, out, lambda gate, ok, desc: recorded.append((gate, ok, desc)), 'unused')
        self.assertEqual([gate for gate, _, _ in recorded], list(app.GATES))
        self.assertEqual(recorded[-1], ('V-SWITCH-MONITOR', False, 'Mirror never applied'))


class Choices(unittest.TestCase):
    def test_presses_follow_the_initial_selection_and_wrap(self):
        # No switch choice in effect counts as after the four.
        self.assertEqual(app.switch_presses('Join Displays', None, False), 1)
        self.assertEqual(app.switch_presses('Mirror', None, False), 2)
        self.assertEqual(app.switch_presses('Join Displays', 'Mirror', False), 1)
        self.assertEqual(app.switch_presses('Mirror', 'Join Displays', False), 1)
        self.assertEqual(app.switch_presses('Mirror', 'Mirror', False), 2)
        # A built-in panel adds External Only and Built-in Only.
        self.assertEqual(app.switch_presses('Join Displays', None, True), 1)
        self.assertEqual(app.switch_presses('Mirror', None, True), 4)
        self.assertEqual(app.switch_presses('Mirror', 'Built-in Only', True), 1)
        self.assertEqual(app.switch_presses('Built-in Only', 'Mirror', True), 3)

    def test_the_popup_is_its_layer_surface(self):
        self.assertTrue(app.popup_open({'layers': [{'namespace': app.SWITCH_POPUP}]}))
        self.assertFalse(app.popup_open({'layers': [{'namespace': 'tuna-switcher'}]}))
        self.assertFalse(app.popup_open({'layers': None}))


class Layouts(unittest.TestCase):
    def test_mirror_mode_is_the_largest_size_every_monitor_offers(self):
        self.assertEqual(app.mirror_mode(joined()), [1920, 1080])
        display = joined()
        display['monitors'][1]['modes'] = [[1024, 768], [800, 600]]
        self.assertEqual(app.mirror_mode(display), [1024, 768])
        display['monitors'][1]['modes'] = [[640, 480]]
        self.assertIsNone(app.mirror_mode(display))

    def test_mirror_is_one_primary_logical_monitor_at_the_common_mode(self):
        self.assertIsNone(app.mirror_problem(mirrored()))
        self.assertIn('2 logical monitors', app.mirror_problem(joined()))
        partial = mirrored()
        partial['logical'][0]['monitors'] = ['Virtual-1']
        self.assertIn('not every monitor', app.mirror_problem(partial))
        moved = mirrored()
        moved['logical'][0]['x'] = 10
        self.assertIn('at 10,0', app.mirror_problem(moved))
        small = mirrored()
        small['monitors'][1]['current'] = [1024, 768]
        self.assertIn("{'Virtual-2': [1024, 768]}", app.mirror_problem(small))
        alone = {'monitors': [dict(FIRST)], 'logical': [logical(0, 0, ['Virtual-1'], True)]}
        self.assertIn('nothing to mirror', app.mirror_problem(alone))

    def test_join_is_side_by_side_at_preferred_modes_primary_first(self):
        self.assertIsNone(app.join_problem(joined()))
        self.assertIn('not one per monitor', app.join_problem(mirrored()))
        overlapping = joined()
        overlapping['logical'][1]['x'] = 0
        overlapping['logical'][1]['y'] = 800
        self.assertIn('not beside the previous at 1280,0', app.join_problem(overlapping))
        secondary_first = joined()
        secondary_first['logical'][0]['primary'], secondary_first['logical'][1]['primary'] = False, True
        self.assertIn('not the first monitor alone', app.join_problem(secondary_first))
        unpreferred = joined()
        unpreferred['monitors'][1]['current'] = [1920, 1080]
        self.assertIn('not its preferred [1024, 768]', app.join_problem(unpreferred))

    def test_join_steps_by_the_logical_width(self):
        three = joined()
        three['monitors'].append({'connector': 'Virtual-3', 'builtin': False, 'preferred': [800, 600],
                                  'current': [800, 600], 'modes': [[800, 600]]})
        # Virtual-1 at scale 2 is 640 wide; Virtual-2 rotated is 768 wide.
        three['logical'] = [logical(0, 0, ['Virtual-1'], True, scale=2.0),
                            logical(640, 0, ['Virtual-2'], transform=1),
                            logical(1408, 0, ['Virtual-3'])]
        self.assertIsNone(app.join_problem(three))
        three['logical'][2]['x'] = 1664
        self.assertIn('Virtual-3 at 1664,0, not beside the previous at 1408,0', app.join_problem(three))


if __name__ == '__main__':
    unittest.main()
