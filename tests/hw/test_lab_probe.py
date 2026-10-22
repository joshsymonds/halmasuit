#!/usr/bin/env python3
"""Tests for nix/lab/lab_probe.py.

The probe is exercised through its functions and its Host seams: a temporary
tree stands in for /sys, /dev, /proc, and the uinput node, a recording fake
stands in for the uinput ioctls, a fake clock stands in for time, and a fake
passwd table stands in for the account database. Nothing here opens a real
device, reads the real /proc or passwd database, or sleeps.
"""

import contextlib
import importlib
import io
import os
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "nix" / "lab"))
probe = importlib.import_module("lab_probe")

PROBE_PID = 4100
UDEV_PID = 311
KEYD_PID = 1140
LOGIND_PID = 702
GREETER_PID = 812
OTHER_USER_PID = 2201
SYSNAME = "input7"
DEVICE = "event5"
DEVICE_NAME = "lab-probe keyboard"
CONSUMER_USER = "greeter"
CONSUMER_UID = 988
ROOT_UID = 0
OTHER_USER_UID = 1000
FAKE_PASSWD = {CONSUMER_USER: CONSUMER_UID}
PASSWORD_CODES = [17, 19, 24, 49, 34, 25, 19, 24, 48, 18]  # w r o n g p r o b e
ENTER = 28
BACKSPACE = 14
CLEAR_PRESSES = 32
TYPED_KEYS = CLEAR_PRESSES + len(PASSWORD_CODES) + 1
ALPHABET = {
    "a": 30, "b": 48, "c": 46, "d": 32, "e": 18, "f": 33, "g": 34,
    "h": 35, "i": 23, "j": 36, "k": 37, "l": 38, "m": 50, "n": 49,
    "o": 24, "p": 25, "q": 16, "r": 19, "s": 31, "t": 20, "u": 22,
    "v": 47, "w": 17, "x": 45, "y": 21, "z": 44,
}
SYNTHETIC_USERNAME = "probeuser"
USERNAME_CODES = [ALPHABET[char] for char in SYNTHETIC_USERNAME]  # p r o b e u s e r
TYPED_KEYS_WITH_USERNAME = CLEAR_PRESSES + len(USERNAME_CODES) + 1 + len(PASSWORD_CODES) + 1


def key_event(code, value):
    return struct.pack("<qqHHi", 0, 0, 1, code, value)


def syn_event():
    return struct.pack("<qqHHi", 0, 0, 0, 0, 0)


def fake_lookup_uid(name):
    return FAKE_PASSWD[name]  # KeyError for an unknown user, as pwd.getpwnam does


class FakeClock:
    """Time that only moves when something sleeps."""

    def __init__(self):
        self.now = 0.0
        self.sleeps = []

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class FakeUinput:
    """Records uinput ioctls; UI_GET_SYSNAME fills in the kernel's sysname."""

    def __init__(self, sysname, fake_clock):
        self.sysname = sysname
        self.fake_clock = fake_clock
        self.requests = []
        self.keybits = []
        self.setup = None
        self.sleep_before_destroy = None

    def __call__(self, fd, request, arg=0):
        self.requests.append(request)
        if request == probe.ui_get_sysname(64):
            name = self.sysname.encode() + b"\0"
            arg[: len(name)] = name
        elif request == probe.UI_SET_KEYBIT:
            self.keybits.append(arg)
        elif request == probe.UI_DEV_SETUP:
            self.setup = bytes(arg)
        elif request == probe.UI_DEV_DESTROY:
            sleeps = self.fake_clock.sleeps
            self.sleep_before_destroy = sleeps[-1] if sleeps else None
        return 0


class HolderClock(FakeClock):
    """A FakeClock whose sleeps let the fake holder answer while the probe waits."""

    def __init__(self):
        super().__init__()
        self.holder = None

    def sleep(self, seconds):
        super().sleep(seconds)
        if self.holder is not None:
            self.holder.step()


class FakeKbdHolder:
    """The boot-time holder: keeps both FIFOs open O_RDWR and answers each request it reads."""

    def __init__(self, tree, reply=None):
        self.request_fd = os.open(tree.request, os.O_RDWR | os.O_NONBLOCK)
        self.reply_fd = os.open(tree.reply, os.O_RDWR | os.O_NONBLOCK)
        self.reply = reply
        self.requests = []

    def step(self):
        try:
            line = os.read(self.request_fd, 4096)
        except BlockingIOError:
            return
        if line:
            self.requests.append(line)
            if self.reply is not None:
                os.write(self.reply_fd, self.reply)

    def close(self):
        os.close(self.request_fd)
        os.close(self.reply_fd)


class Tree:
    """Temporary sysfs, dev, proc, run, and uinput paths for one probe run."""

    def __init__(self, root):
        self.root = Path(root)
        self.uinput = self.root / "uinput"
        self.uinput.touch()
        self.dev = self.root / "dev"
        self.dev.mkdir()
        self.sys = self.root / "sys"
        self.sys.mkdir()
        self.proc = self.root / "proc"
        self.proc.mkdir()
        self.run = self.root / "run"
        self.run.mkdir()
        self.request = self.run / "request"
        self.reply = self.run / "reply"
        os.mkfifo(self.request)
        os.mkfifo(self.reply)

    @property
    def node(self):
        return str(self.dev / DEVICE)

    def add_input(self, sysname, event=None):
        input_dir = self.sys / "devices" / "virtual" / "input" / sysname
        input_dir.mkdir(parents=True)
        if event is not None:
            (input_dir / event).mkdir()
            (self.dev / event).touch()

    def add_input_device(self, sysname, name, event):
        """An input device as the kernel publishes it: its devices/ directory with a name file and an
        eventN child, a class/input symlink to that directory, and a class/input symlink to the event."""
        self.add_input(sysname, event)
        (self.sys / "devices" / "virtual" / "input" / sysname / "name").write_text(f"{name}\n")
        class_input = self.sys / "class" / "input"
        class_input.mkdir(parents=True, exist_ok=True)
        os.symlink(f"../../devices/virtual/input/{sysname}", class_input / sysname)
        os.symlink(f"../../devices/virtual/input/{sysname}/{event}", class_input / event)

    def add_process(self, pid, comm, uid, fd_targets):
        proc = self.proc / str(pid)
        (proc / "fd").mkdir(parents=True)
        (proc / "comm").write_text(comm + "\n")
        (proc / "status").write_text(
            f"Name:\t{comm}\nState:\tS (sleeping)\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"
        )
        for number, target in enumerate(fd_targets):
            os.symlink(target, proc / "fd" / str(number))

    def host(self, ioctl, clock):
        return probe.Host(
            uinput_path=str(self.uinput),
            dev_root=str(self.dev),
            sysfs_root=str(self.sys),
            proc_root=str(self.proc),
            holder_request=str(self.request),
            holder_reply=str(self.reply),
            self_pid=PROBE_PID,
            ioctl=ioctl,
            clock=clock.clock,
            sleep=clock.sleep,
            lookup_uid=fake_lookup_uid,
        )


class KeyMap(unittest.TestCase):
    def test_alphabet_matches_kernel_key_codes(self):
        self.assertEqual(probe.KEY_CODES, ALPHABET)

    def test_password_is_lowercase_ascii_letters(self):
        self.assertTrue(probe.PASSWORD)
        self.assertTrue(probe.PASSWORD.isascii())
        self.assertTrue(probe.PASSWORD.isalpha())
        self.assertTrue(probe.PASSWORD.islower())

    def test_key_sequence_maps_each_character_then_enter(self):
        self.assertEqual(probe.key_sequence("wrongprobe"), PASSWORD_CODES + [ENTER])

    def test_unmapped_character_is_refused(self):
        with self.assertRaises(KeyError):
            probe.key_sequence("A")


class KernelAbi(unittest.TestCase):
    def test_uinput_ioctl_numbers(self):
        self.assertEqual(probe.UI_SET_EVBIT, 0x40045564)
        self.assertEqual(probe.UI_SET_KEYBIT, 0x40045565)
        self.assertEqual(probe.UI_DEV_SETUP, 0x405C5503)
        self.assertEqual(probe.UI_DEV_CREATE, 0x5501)
        self.assertEqual(probe.UI_DEV_DESTROY, 0x5502)

    def test_ui_get_sysname_encodes_length(self):
        self.assertEqual(probe.ui_get_sysname(64), 0x8040552C)

    def test_input_event_bytes(self):
        self.assertEqual(
            probe.pack_event(1, 30, 1),
            b"\x00" * 16 + b"\x01\x00" + b"\x1e\x00" + b"\x01\x00\x00\x00",
        )
        self.assertEqual(probe.pack_event(0, 0, 0), bytes(24))

    def test_uinput_setup_bytes(self):
        name = "lab-probe keyboard"
        setup = probe.pack_setup(name)
        self.assertEqual(len(setup), 92)
        self.assertEqual(setup[0:2], b"\x06\x00")
        self.assertEqual(setup[8 : 8 + len(name)], name.encode())
        self.assertEqual(setup[8 + len(name) :], bytes(92 - 8 - len(name)))


class HolderDetection(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tree = Tree(self._tmp.name)
        self.node = self.tree.node

    def holder(self):
        return probe.holder_of(str(self.tree.proc), self.node, PROBE_PID, CONSUMER_UID)

    def test_finds_consumer_process_holding_node(self):
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [self.node])
        self.assertEqual(self.holder(), probe.Holder(GREETER_PID, "halmasuit"))

    def test_skips_the_probe_itself(self):
        self.tree.add_process(PROBE_PID, "python3", CONSUMER_UID, [self.node])
        self.assertIsNone(self.holder())

    def test_skips_udev_processes(self):
        self.tree.add_process(UDEV_PID, "systemd-udevd", CONSUMER_UID, [self.node])
        self.tree.add_process(UDEV_PID + 1, "(udev-worker)", CONSUMER_UID, [self.node])
        self.assertIsNone(self.holder())

    def test_ignores_other_nodes(self):
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [str(self.tree.dev / "event9")])
        self.assertIsNone(self.holder())

    def test_none_when_nothing_holds_node(self):
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [])
        self.assertIsNone(self.holder())

    def test_root_owned_keyd_does_not_count(self):
        self.tree.add_process(KEYD_PID, "keyd", ROOT_UID, [self.node])
        self.assertIsNone(self.holder())

    def test_root_owned_logind_does_not_count(self):
        self.tree.add_process(LOGIND_PID, "systemd-logind", ROOT_UID, [self.node])
        self.assertIsNone(self.holder())

    def test_other_non_consumer_user_does_not_count(self):
        self.tree.add_process(OTHER_USER_PID, "someapp", OTHER_USER_UID, [self.node])
        self.assertIsNone(self.holder())

    def test_consumer_holder_is_found_past_root_holders(self):
        self.tree.add_process(KEYD_PID, "keyd", ROOT_UID, [self.node])
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [self.node])
        self.assertEqual(self.holder(), probe.Holder(GREETER_PID, "halmasuit"))

    def test_process_without_status_file_is_skipped(self):
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [self.node])
        (self.tree.proc / str(GREETER_PID) / "status").unlink()
        self.assertIsNone(self.holder())


class Waiting(unittest.TestCase):
    def test_returns_first_value_the_check_produces(self):
        clock = FakeClock()
        answers = iter([None, None, "ready"])
        result = probe.wait_until(
            lambda: next(answers), timeout=30, clock=clock.clock, sleep=clock.sleep
        )
        self.assertEqual(result, "ready")
        self.assertEqual(len(clock.sleeps), 2)

    def test_gives_up_after_timeout(self):
        clock = FakeClock()
        result = probe.wait_until(
            lambda: None, timeout=30, clock=clock.clock, sleep=clock.sleep
        )
        self.assertIsNone(result)
        self.assertGreaterEqual(clock.now, 30)


class SysfsNode(unittest.TestCase):
    def test_event_node_name_comes_from_input_directory(self):
        with tempfile.TemporaryDirectory() as root:
            tree = Tree(root)
            tree.add_input(SYSNAME, DEVICE)
            self.assertEqual(probe.event_node_name(str(tree.sys), SYSNAME), DEVICE)

    def test_event_node_name_is_none_before_input_exists(self):
        with tempfile.TemporaryDirectory() as root:
            tree = Tree(root)
            self.assertIsNone(probe.event_node_name(str(tree.sys), SYSNAME))

    def test_real_host_looks_for_event_nodes_under_dev_input(self):
        # udev creates evdev nodes as /dev/input/eventN; the tree fakes that directory.
        self.assertEqual(probe.Host().dev_root, "/dev/input")


class ConsumerArgument(unittest.TestCase):
    def test_consumer_user_is_parsed_from_argv(self):
        args = probe.parse_arguments(["--consumer-user", CONSUMER_USER])
        self.assertEqual(args.consumer_user, CONSUMER_USER)

    def test_consumer_user_is_required(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                probe.parse_arguments([])
        self.assertEqual(raised.exception.code, 2)

    def test_username_is_parsed_from_argv(self):
        args = probe.parse_arguments(
            ["--consumer-user", CONSUMER_USER, "--username", SYNTHETIC_USERNAME]
        )
        self.assertEqual(args.username, SYNTHETIC_USERNAME)

    def test_username_defaults_to_empty_when_absent(self):
        args = probe.parse_arguments(["--consumer-user", CONSUMER_USER])
        self.assertEqual(args.username, "")

    def test_early_keyboard_flag_is_parsed(self):
        args = probe.parse_arguments(["--consumer-user", CONSUMER_USER, "--early-keyboard"])
        self.assertTrue(args.early_keyboard)

    def test_early_keyboard_is_off_when_absent(self):
        args = probe.parse_arguments(["--consumer-user", CONSUMER_USER])
        self.assertFalse(args.early_keyboard)


class ProbeRun(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tree = Tree(self._tmp.name)
        self.clock = FakeClock()

    def run_probe(self, ioctl, username=""):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = probe.run(self.tree.host(ioctl, self.clock), CONSUMER_USER, username)
        return status, out.getvalue().splitlines()

    def add_ready_keyboard(self):
        self.tree.add_input(SYSNAME, DEVICE)
        self.tree.add_process(PROBE_PID, "python3", CONSUMER_UID, [self.tree.node])
        self.tree.add_process(UDEV_PID, "systemd-udevd", ROOT_UID, [self.tree.node])
        self.tree.add_process(KEYD_PID, "keyd", ROOT_UID, [self.tree.node])
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [self.tree.node])

    def test_success_clears_field_then_types_password_and_reports_three_lines(self):
        self.add_ready_keyboard()
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl)

        self.assertEqual(status, 0)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"ready holder=halmasuit pid={GREETER_PID}",
                f"typed keys={TYPED_KEYS}",
            ],
        )
        self.assertEqual(TYPED_KEYS, 43)
        expected = []
        for code in [BACKSPACE] * CLEAR_PRESSES + PASSWORD_CODES + [ENTER]:
            expected += [key_event(code, 1), syn_event(), key_event(code, 0), syn_event()]
        self.assertEqual(self.tree.uinput.read_bytes(), b"".join(expected))
        self.assertEqual(
            sorted(set(ioctl.keybits)),
            sorted(set(PASSWORD_CODES + [ENTER, BACKSPACE])),
        )
        self.assertEqual(ioctl.requests[0], probe.UI_SET_EVBIT)
        self.assertIn(probe.UI_DEV_CREATE, ioctl.requests)
        self.assertEqual(ioctl.requests[-1], probe.UI_DEV_DESTROY)
        self.assertEqual(ioctl.sleep_before_destroy, 1.0)

    def test_empty_username_types_todays_sequence(self):
        self.add_ready_keyboard()
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl, username="")

        self.assertEqual(status, 0)
        self.assertEqual(lines[-1], f"typed keys={TYPED_KEYS}")
        expected = []
        for code in [BACKSPACE] * CLEAR_PRESSES + PASSWORD_CODES + [ENTER]:
            expected += [key_event(code, 1), syn_event(), key_event(code, 0), syn_event()]
        self.assertEqual(self.tree.uinput.read_bytes(), b"".join(expected))

    def test_username_is_typed_with_enter_before_password_with_enter(self):
        self.add_ready_keyboard()
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl, username=SYNTHETIC_USERNAME)

        self.assertEqual(status, 0)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"ready holder=halmasuit pid={GREETER_PID}",
                f"typed keys={TYPED_KEYS_WITH_USERNAME}",
            ],
        )
        self.assertEqual(TYPED_KEYS_WITH_USERNAME, 53)
        expected = []
        sequence = (
            [BACKSPACE] * CLEAR_PRESSES
            + USERNAME_CODES + [ENTER]
            + PASSWORD_CODES + [ENTER]
        )
        for code in sequence:
            expected += [key_event(code, 1), syn_event(), key_event(code, 0), syn_event()]
        self.assertEqual(self.tree.uinput.read_bytes(), b"".join(expected))
        self.assertEqual(
            sorted(set(ioctl.keybits)),
            sorted(set(PASSWORD_CODES + USERNAME_CODES + [ENTER, BACKSPACE])),
        )
        self.assertEqual(ioctl.requests[-1], probe.UI_DEV_DESTROY)

    def test_unmappable_username_is_refused_before_uinput_opens(self):
        self.tree.uinput.unlink()
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl, username="probe2")

        self.assertEqual(status, 1)
        self.assertEqual(lines, ["error reason=username has a character with no key code"])
        self.assertEqual(ioctl.requests, [])

    def test_node_timeout_reports_one_error_line_and_destroys(self):
        self.tree.add_input(SYSNAME)
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl)

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith("error reason="))
        self.assertEqual(ioctl.requests[-1], probe.UI_DEV_DESTROY)

    def test_holder_timeout_when_only_probe_and_udev_hold_node(self):
        self.tree.add_input(SYSNAME, DEVICE)
        self.tree.add_process(PROBE_PID, "python3", CONSUMER_UID, [self.tree.node])
        self.tree.add_process(UDEV_PID, "(udev-worker)", ROOT_UID, [self.tree.node])
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl)

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0], f"device node={self.tree.node}")
        self.assertTrue(lines[1].startswith("error reason="))
        self.assertEqual(ioctl.requests[-1], probe.UI_DEV_DESTROY)
        self.assertGreaterEqual(self.clock.now, 30)

    def test_root_holders_alone_do_not_make_keyboard_ready(self):
        self.tree.add_input(SYSNAME, DEVICE)
        self.tree.add_process(KEYD_PID, "keyd", ROOT_UID, [self.tree.node])
        self.tree.add_process(LOGIND_PID, "systemd-logind", ROOT_UID, [self.tree.node])
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl)

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0], f"device node={self.tree.node}")
        self.assertTrue(lines[1].startswith("error reason="))
        self.assertEqual(ioctl.requests[-1], probe.UI_DEV_DESTROY)
        self.assertGreaterEqual(self.clock.now, 30)

    def test_unknown_consumer_user_reports_one_error_line_before_touching_uinput(self):
        self.tree.add_input(SYSNAME, DEVICE)
        ioctl = FakeUinput(SYSNAME, self.clock)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = probe.run(self.tree.host(ioctl, self.clock), "no-such-user")

        self.assertEqual(status, 1)
        lines = out.getvalue().splitlines()
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith("error reason="))
        self.assertEqual(ioctl.requests, [])

    def test_missing_uinput_reports_error_and_status_one(self):
        self.tree.uinput.unlink()
        ioctl = FakeUinput(SYSNAME, self.clock)
        status, lines = self.run_probe(ioctl)

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith("error reason="))
        self.assertEqual(ioctl.requests, [])


OTHER_INPUT_NAME = "AT Translated Set 2 keyboard"
NEAR_MISS_NAME = "lab-probe keyboard spare"


def ok_reply(count):
    return f"ok {count}\n".encode()


class EarlyKeyboard(unittest.TestCase):
    """--early-keyboard: the probe finds the holder's node by name and asks the holder to type."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tree = Tree(self._tmp.name)
        self.tree.uinput.unlink()  # early mode must never open /dev/uinput
        self.clock = HolderClock()
        self.ioctl = FakeUinput(SYSNAME, self.clock)
        self.holder = None

    def add_other_inputs(self):
        self.tree.add_input_device("input3", OTHER_INPUT_NAME, "event2")
        self.tree.add_input_device("input4", NEAR_MISS_NAME, "event6")

    def add_probe_keyboard(self):
        self.tree.add_input_device(SYSNAME, DEVICE_NAME, DEVICE)

    def add_consumer_holder(self):
        self.tree.add_process(GREETER_PID, "halmasuit", CONSUMER_UID, [self.tree.node])

    def add_ready_early_keyboard(self):
        self.add_other_inputs()
        self.add_probe_keyboard()
        self.add_consumer_holder()

    def open_holder(self, reply):
        self.holder = FakeKbdHolder(self.tree, reply)
        self.addCleanup(self.holder.close)
        self.clock.holder = self.holder

    def run_early(self, username=""):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = probe.run(
                self.tree.host(self.ioctl, self.clock), CONSUMER_USER, username, early_keyboard=True
            )
        return status, out.getvalue().splitlines()

    def test_named_node_is_found_among_other_inputs_without_touching_uinput(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS))
        status, lines = self.run_early()

        self.assertEqual(status, 0)
        self.assertEqual(lines[0], f"device node={self.tree.node}")
        self.assertEqual(self.ioctl.requests, [])
        self.assertFalse(self.tree.uinput.exists())

    def test_undecodable_name_before_the_probe_keyboard_is_skipped(self):
        self.tree.add_input_device("input2", "placeholder", "event1")
        (self.tree.sys / "devices" / "virtual" / "input" / "input2" / "name").write_bytes(b"bad\xff\xfe\n")
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS))
        status, lines = self.run_early()

        self.assertEqual(status, 0)
        self.assertEqual(lines[0], f"device node={self.tree.node}")

    def test_near_miss_name_is_not_the_probe_keyboard(self):
        self.add_other_inputs()
        self.add_consumer_holder()
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(lines, [f"error reason=no input device named {DEVICE_NAME} within 30s"])
        self.assertGreaterEqual(self.clock.now, 30)

    def test_request_line_is_exact_without_username(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS))
        self.run_early()

        self.assertEqual(self.holder.requests, [b"\n"])

    def test_request_line_carries_username(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS_WITH_USERNAME))
        status, lines = self.run_early(SYNTHETIC_USERNAME)

        self.assertEqual(status, 0)
        self.assertEqual(self.holder.requests, [SYNTHETIC_USERNAME.encode() + b"\n"])

    def test_ok_reply_logs_device_ready_and_typed_lines_in_order(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS))
        status, lines = self.run_early()

        self.assertEqual(status, 0)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"ready holder=halmasuit pid={GREETER_PID}",
                f"typed keys={TYPED_KEYS}",
            ],
        )

    def test_ok_reply_with_username_logs_the_username_count(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS_WITH_USERNAME))
        status, lines = self.run_early(SYNTHETIC_USERNAME)

        self.assertEqual(status, 0)
        self.assertEqual(lines[-1], f"typed keys={TYPED_KEYS_WITH_USERNAME}")

    def test_holder_error_reply_is_one_error_line(self):
        self.add_ready_early_keyboard()
        self.open_holder(b"error keyboard busy\n")
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"ready holder=halmasuit pid={GREETER_PID}",
                "error reason=holder: keyboard busy",
            ],
        )

    def test_wrong_count_reply_is_one_error_line(self):
        self.add_ready_early_keyboard()
        self.open_holder(ok_reply(TYPED_KEYS - 1))
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 3)
        self.assertEqual(
            lines[-1],
            f"error reason=holder typed {TYPED_KEYS - 1} keys, expected {TYPED_KEYS}",
        )

    def test_malformed_reply_is_one_error_line(self):
        self.add_ready_early_keyboard()
        self.open_holder(b"ok forty-three\n")
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 3)
        self.assertTrue(lines[-1].startswith("error reason=malformed holder reply"))

    def test_silent_holder_times_out_as_one_error_line(self):
        self.add_ready_early_keyboard()
        self.open_holder(None)
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(len(lines), 3)
        self.assertEqual(lines[-1], "error reason=no holder reply within 30s")
        self.assertGreaterEqual(self.clock.now, 30)

    def test_no_reader_on_request_is_one_error_line(self):
        self.add_ready_early_keyboard()
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"ready holder=halmasuit pid={GREETER_PID}",
                f"error reason=no reader on {self.tree.request}",
            ],
        )

    def test_missing_named_node_is_one_error_line_after_timeout(self):
        self.add_other_inputs()
        self.add_consumer_holder()
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(lines, [f"error reason=no input device named {DEVICE_NAME} within 30s"])
        self.assertGreaterEqual(self.clock.now, 30)

    def test_no_consumer_holder_is_one_error_line_after_timeout(self):
        self.add_other_inputs()
        self.add_probe_keyboard()
        status, lines = self.run_early()

        self.assertEqual(status, 1)
        self.assertEqual(
            lines,
            [
                f"device node={self.tree.node}",
                f"error reason=no consumer-user process holds {self.tree.node} within 30s",
            ],
        )
        self.assertGreaterEqual(self.clock.now, 30)


if __name__ == "__main__":
    unittest.main()
