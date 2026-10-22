#!/usr/bin/env python3
"""Lab-image keyboard probe: types one wrong password into the greeter.

Creates a uinput virtual keyboard, waits for udev to publish its event node
and for a process other than udev, running as the consumer user, to hold that
node open, types PASSWORD and Enter, holds the device briefly, then destroys
it. PASSWORD is a fixed, obviously wrong constant: it is never read from
anywhere and reaches the system only as key events.

Usage: lab_probe.py --consumer-user USER

On success prints `device node=`, `ready holder=`, and `typed keys=`, one
line each. On failure prints one `error reason=` line and exits with status 1.
"""

import argparse
import dataclasses
import fcntl
import os
import pwd
import re
import struct
import sys
import time
from collections.abc import Callable, Iterable
from pathlib import Path

PASSWORD = "wrongprobe"
DEVICE_NAME = "lab-probe keyboard"
VENDOR = 0x0001
PRODUCT = 0x0001
VERSION = 0x0001
UDEV_COMMS = frozenset({"systemd-udevd", "(udev-worker)"})
WAIT_SECONDS = 30
POLL_SECONDS = 0.05
KEY_PACE_SECONDS = 0.02
HOLD_SECONDS = 1.0

KEY_CODES = {
    "a": 30, "b": 48, "c": 46, "d": 32, "e": 18, "f": 33, "g": 34,
    "h": 35, "i": 23, "j": 36, "k": 37, "l": 38, "m": 50, "n": 49,
    "o": 24, "p": 25, "q": 16, "r": 19, "s": 31, "t": 20, "u": 22,
    "v": 47, "w": 17, "x": 45, "y": 21, "z": 44,
}
ENTER = 28

EV_SYN = 0
SYN_REPORT = 0
EV_KEY = 1
BUS_VIRTUAL = 0x06

UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_DEV_SETUP = 0x405C5503
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502

EVENT = struct.Struct("<qqHHi")
SETUP = struct.Struct("<HHHH80sI")
SYN_EVENT = EVENT.pack(0, 0, EV_SYN, SYN_REPORT, 0)
EVENT_NODE = re.compile(r"event\d+")


class ProbeError(Exception):
    """A step could not finish; the message is the error reason."""


@dataclasses.dataclass(frozen=True)
class Holder:
    pid: int
    comm: str


def passwd_uid(user: str) -> int:
    return pwd.getpwnam(user).pw_uid


@dataclasses.dataclass(frozen=True)
class Host:
    """Every path, ioctl, clock, and account lookup the probe touches; main() uses the defaults."""

    uinput_path: str = "/dev/uinput"
    dev_root: str = "/dev/input"
    sysfs_root: str = "/sys"
    proc_root: str = "/proc"
    self_pid: int = dataclasses.field(default_factory=os.getpid)
    ioctl: Callable[..., object] = fcntl.ioctl
    clock: Callable[[], float] = time.monotonic
    sleep: Callable[[float], None] = time.sleep
    lookup_uid: Callable[[str], int] = passwd_uid


def ui_get_sysname(length: int) -> int:
    return 0x8000552C | (length << 16)


def key_sequence(text: str) -> list[int]:
    return [KEY_CODES[char] for char in text] + [ENTER]


def pack_event(event_type: int, code: int, value: int) -> bytes:
    return EVENT.pack(0, 0, event_type, code, value)


def pack_setup(name: str) -> bytes:
    return SETUP.pack(BUS_VIRTUAL, VENDOR, PRODUCT, VERSION, name.encode(), 0)


def event_node_name(sysfs_root: str, sysname: str) -> str | None:
    try:
        entries = os.listdir(os.path.join(sysfs_root, "devices/virtual/input", sysname))
    except FileNotFoundError:
        return None
    return next((entry for entry in entries if EVENT_NODE.fullmatch(entry)), None)


def real_uid(proc: str) -> int | None:
    for line in Path(proc, "status").read_text().splitlines():
        if line.startswith("Uid:"):
            return int(line.split()[1])
    return None


def holder_of(proc_root: str, node: str, self_pid: int, consumer_uid: int) -> Holder | None:
    for entry in os.listdir(proc_root):
        if not entry.isdigit() or int(entry) == self_pid:
            continue
        proc = os.path.join(proc_root, entry)
        try:
            comm = Path(proc, "comm").read_text().strip()
            if comm in UDEV_COMMS or real_uid(proc) != consumer_uid:
                continue
            fds = os.listdir(os.path.join(proc, "fd"))
        except OSError:
            continue  # the process exited between listing and reading it
        for fd in fds:
            try:
                target = os.readlink(os.path.join(proc, "fd", fd))
            except OSError:
                continue
            if target == node:
                return Holder(int(entry), comm)
    return None


def wait_until(
    check: Callable[[], object],
    *,
    timeout: float,
    clock: Callable[[], float],
    sleep: Callable[[float], None],
) -> object:
    deadline = clock() + timeout
    while True:
        value = check()
        if value is not None:
            return value
        if clock() >= deadline:
            return None
        sleep(POLL_SECONDS)


def type_keys(write: Callable[[bytes], object], codes: Iterable[int], sleep: Callable[[float], None]) -> None:
    for code in codes:
        write(pack_event(EV_KEY, code, 1))
        write(SYN_EVENT)
        write(pack_event(EV_KEY, code, 0))
        write(SYN_EVENT)
        sleep(KEY_PACE_SECONDS)


def wait_for_node(host: Host, sysname: str) -> str:
    def check() -> str | None:
        name = event_node_name(host.sysfs_root, sysname)
        if name is None:
            return None
        path = os.path.join(host.dev_root, name)
        return path if os.path.exists(path) else None

    node = wait_until(check, timeout=WAIT_SECONDS, clock=host.clock, sleep=host.sleep)
    if node is None:
        raise ProbeError(f"no event node for {sysname} within {WAIT_SECONDS}s")
    return node


def wait_for_holder(host: Host, node: str, consumer_uid: int) -> Holder:
    holder = wait_until(
        lambda: holder_of(host.proc_root, node, host.self_pid, consumer_uid),
        timeout=WAIT_SECONDS,
        clock=host.clock,
        sleep=host.sleep,
    )
    if holder is None:
        raise ProbeError(f"no consumer-user process holds {node} within {WAIT_SECONDS}s")
    return holder


def probe_keyboard(host: Host, fd: int, consumer_uid: int) -> None:
    codes = key_sequence(PASSWORD)
    host.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    for code in sorted(set(codes)):
        host.ioctl(fd, UI_SET_KEYBIT, code)
    host.ioctl(fd, UI_DEV_SETUP, pack_setup(DEVICE_NAME))
    host.ioctl(fd, UI_DEV_CREATE, 0)
    try:
        sysname_buf = bytearray(64)
        host.ioctl(fd, ui_get_sysname(64), sysname_buf)
        sysname = bytes(sysname_buf).split(b"\0", 1)[0].decode()

        node = wait_for_node(host, sysname)
        print(f"device node={node}", flush=True)
        holder = wait_for_holder(host, node, consumer_uid)
        print(f"ready holder={holder.comm} pid={holder.pid}", flush=True)

        type_keys(lambda event: os.write(fd, event), codes, host.sleep)
        print(f"typed keys={len(codes)}", flush=True)
        host.sleep(HOLD_SECONDS)
    finally:
        host.ioctl(fd, UI_DEV_DESTROY, 0)


def fail(reason: str) -> int:
    print(f"error reason={reason}", flush=True)
    return 1


def run(host: Host, consumer_user: str) -> int:
    try:
        consumer_uid = host.lookup_uid(consumer_user)
    except KeyError:
        return fail(f"unknown consumer user {consumer_user}")
    try:
        fd = os.open(host.uinput_path, os.O_WRONLY | os.O_NONBLOCK)
    except OSError as err:
        return fail(f"open {host.uinput_path}: {err}")
    try:
        probe_keyboard(host, fd, consumer_uid)
    except (OSError, ProbeError) as err:
        return fail(str(err))
    finally:
        os.close(fd)
    return 0


def parse_consumer_user(argv: list[str] | None) -> str:
    parser = argparse.ArgumentParser(description="Type one wrong password into the greeter through a virtual keyboard.")
    parser.add_argument("--consumer-user", required=True, metavar="USER", help="account whose process must hold the keyboard")
    return parser.parse_args(argv).consumer_user


def main(argv: list[str] | None = None) -> int:
    return run(Host(), parse_consumer_user(argv))


if __name__ == "__main__":
    sys.exit(main())
