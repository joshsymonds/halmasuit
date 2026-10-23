#!/usr/bin/env python3
"""An absolute uinput pointer (like QEMU's usb-tablet) that moves to X Y on
halmasuit's primary output and, with `click`, presses and releases the left
button.

Usage: uinput_pointer.py OUTPUT_W OUTPUT_H X Y [click|move]

Waits up to 30 seconds for halmasuit to hold the device's event node (its
input hotplug adds it) before moving; exits 1 if it never does. Prints one
line per step.
"""
import fcntl
import os
import struct
import sys
import time

EV_SYN, EV_KEY, EV_ABS = 0, 1, 3
SYN_REPORT = 0
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE = 0x110, 0x111, 0x112
ABS_X, ABS_Y = 0, 1
ABS_MAX = 32767
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_SET_ABSBIT = 0x40045567
UI_DEV_SETUP = 0x405C5503
UI_ABS_SETUP = 0x401C5504
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502
UI_GET_SYSNAME_64 = 0x8040552C
EVENT = struct.Struct("<qqHHi")
SETUP = struct.Struct("<HHHH80sI")
ABS_SETUP = struct.Struct("<H2x6i")


def emit(fd, etype, code, value):
    os.write(fd, EVENT.pack(0, 0, etype, code, value))


def syn(fd):
    emit(fd, EV_SYN, SYN_REPORT, 0)


def halmasuit_holds(node):
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            if open(f"/proc/{pid}/comm").read().strip() != "halmasuit":
                continue
            for fd in os.listdir(f"/proc/{pid}/fd"):
                if os.readlink(f"/proc/{pid}/fd/{fd}").endswith(node):
                    return pid
        except OSError:
            continue
    return None


def main():
    out_w, out_h, x, y = (int(a) for a in sys.argv[1:5])
    action = sys.argv[5] if len(sys.argv) > 5 else "click"
    fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    for b in (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE):
        fcntl.ioctl(fd, UI_SET_KEYBIT, b)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_ABS)
    for axis in (ABS_X, ABS_Y):
        fcntl.ioctl(fd, UI_SET_ABSBIT, axis)
        fcntl.ioctl(fd, UI_ABS_SETUP, ABS_SETUP.pack(axis, 0, 0, ABS_MAX, 0, 0, 0))
    fcntl.ioctl(fd, UI_DEV_SETUP, SETUP.pack(0x06, 0x0001, 0x0002, 1, b"uinput pointer", 0))
    fcntl.ioctl(fd, UI_DEV_CREATE, 0)
    try:
        buf = bytearray(64)
        fcntl.ioctl(fd, UI_GET_SYSNAME_64, buf)
        sysname = buf.split(b"\0", 1)[0].decode()
        deadline = time.monotonic() + 30
        node = None
        while time.monotonic() < deadline and node is None:
            base = f"/sys/devices/virtual/input/{sysname}"
            events = [e for e in os.listdir(base) if e.startswith("event")] if os.path.isdir(base) else []
            node = f"/input/{events[0]}" if events else None
            if node is None:
                time.sleep(0.05)
        print(f"device {sysname} node={node}", flush=True)
        holder = None
        while time.monotonic() < deadline and holder is None:
            holder = halmasuit_holds(node)
            if holder is None:
                time.sleep(0.1)
        print(f"held by halmasuit pid={holder}", flush=True)
        if holder is None:
            sys.exit(1)
        time.sleep(0.5)
        ax = round(x * ABS_MAX / out_w)
        ay = round(y * ABS_MAX / out_h)
        # Approach in steps so the client sees motion over the target.
        for i in range(1, 11):
            emit(fd, EV_ABS, ABS_X, round(ax * i / 10))
            emit(fd, EV_ABS, ABS_Y, round(ay * i / 10))
            syn(fd)
            time.sleep(0.03)
        print(f"moved to ({x},{y}) abs=({ax},{ay})", flush=True)
        if action == "click":
            time.sleep(0.2)
            emit(fd, EV_KEY, BTN_LEFT, 1)
            syn(fd)
            time.sleep(0.08)
            emit(fd, EV_KEY, BTN_LEFT, 0)
            syn(fd)
            print("clicked", flush=True)
        time.sleep(1.0)
    finally:
        fcntl.ioctl(fd, UI_DEV_DESTROY, 0)
        os.close(fd)


main()
