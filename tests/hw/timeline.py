#!/usr/bin/env python3
"""Builds one power-on-clock timeline from a gnomon lab run and checks it against a profile.

A run directory holds the files the lab runner collects: `journal.json` (the
boot's `journalctl -o json` output, one object per line) and `firmware/` (FPDT,
the systemd-boot EFI variables, BGRT, and boot_params). Any input may be
missing. test_timeline.py writes the run directories it reads.

Clock. Every time on the timeline is microseconds from the platform timer's
zero, the CPU reset that FPDT and systemd-boot both count from. The kernel's
CLOCK_MONOTONIC zero sits at FPDT `exitbootservice_end` when that record is
present and nonzero, and otherwise at `LoaderTimeExecUSec`. A journal line's
time is that kernel zero plus the line's monotonic time, and it is null when
neither anchor exists. A journal line takes `_SOURCE_MONOTONIC_TIMESTAMP` when
present (kernel lines carry the kmsg time there) and `__MONOTONIC_TIMESTAMP`
otherwise. A lab-crc `start`, `sample`, or `error` line takes its monotonic time
from its own `t=` field, which wins over the journal timestamps; any other
lab-crc line takes the journal timestamps.

Events. Firmware and loader times are events on the platform clock with a null
`mono_us`. A kernel-transport line matching the display pattern is a
`driver-display` event when it also matches the driver pattern and a
`kernel-display` event otherwise. A lab-crc `start` is a `sampler-start` event;
a `sample` or `error` is a `crc` event. Any other lab-crc line is the sampler's
own failure output (`lab-crc: <what>`, optionally `: <strerror>`), kept as a
`crc` event whose source is its unit and which is attributed to no head. A
`start`, `sample`, or `error` line whose fields do not parse is a malformed run.
A lab-vkms-crc `start` is a `vkms-sampler-start` event. A `sample` is a `vkms-crc`
event carrying `head` (`<card>/crtc<id>`), `crc` (eight hex digits), and `active`
(a bool from 0 or 1). An `error` is a `vkms-crc` event carrying `head` and `errno`.
Any other lab-vkms-crc line is the sampler's failure output, a `vkms-crc` event
with no `head`. Each of these takes its monotonic time from `t=`, as lab-crc does.
A lab-vkms-crc `start`, `sample`, or `error` whose fields do not parse is a malformed run.
A lab-probe line is a `probe` event.
A `halmasuit.service` line whose MESSAGE is a tracing JSON line carrying a
halmasuit Event is a `halmasuit` event: its detail is the Event's `event` tag,
its payload is the whole Event object, and its time is the line's monotonic
time on the kernel clock. Every such line is kept, repeats included. Any other
line from that unit is ignored, and lines from other units produce no
`halmasuit` or `gl` event.
Every `halmasuit` event carries `pid`: the line's journal `_PID` as an int, or
null when the line has none. A `halmasuit.service` line whose MESSAGE is a tracing
JSON line from target `smithay::backend::renderer::gles`, with `fields.message`
beginning `GL Vendor: `, `GL Renderer: `, or `GL Version: `, is a `gl` event: its
detail is that message and it carries `pid` likewise.
greetd's start is the first entry whose MESSAGE_ID is the unit-started ID and
whose `UNIT` is `greetd.service`; it is a `unit` event. That start is read from
MESSAGE_ID and UNIT and does not depend on MESSAGE. A journal entry whose MESSAGE
is null, missing, or neither a string nor a list of byte values 0-255 yields no
event derived from its message, of any kind. A list of byte values is decoded as
UTF-8 with replacement and then treated as a string.

Greeter first appearance. Per head, the first sample whose monotonic time is
strictly after greetd's start.

Profiles map a name to a function that returns failure messages for a timeline.
`main` writes `timeline.json` into the run directory, prints each failure, and
exits 0 only when the chosen profile passes.

Profile `bios-shader-greeter` checks a run whose greeter is drawn by halmasuit
itself after the firmware console. M is the first `drm_master_acquired` phase
with a monotonic time. Display lines are checked against M, so the NVIDIA
driver may load before M but may not take the console over before it. The
checks, one message each when they fail: M exists; no console switch, no fbcon
primary-device claim, and no NVIDIA frame-buffer-device line before M; the
firmware mode and BGRT logo are present; at least one frame is not degenerate;
`assert_animating` passes over every halmasuit payload; every head with a
sample after M has a nonzero checksum there and at least two distinct values;
an overlay `client_first_frame` and a greeter `foreground_changed` exist; no
sample from the greeter's foreground until the next session foreground is zero;
`assert_no_flash_stream` passes; no sample after the first `frame_rendered` is
zero. A display line with no monotonic time counts as before M.

Profile `production` checks a run whose halmasuit build emits no `frame_rendered`.
It applies the same M rule and the same messages as `bios-shader-greeter` to
the checks it shares with it: no console switch, fbcon primary-device claim, or
NVIDIA frame-buffer-device line before M; at least one head active after M, with
a nonzero checksum and at least two distinct values there; an overlay
`client_first_frame` and a greeter `foreground_changed`. It checks nothing else:
no firmware mode, BGRT logo, frame stats, `assert_animating`, flash stream,
greeter-window checksum, or checksum after the first frame.

Profile `attended-login` checks a run in which a person logs in at the greeter
through halmasuit. It reads only halmasuit events. The checks, one message each
when they fail: a `session_opened` exists; a `session_client_first_frame` exists;
exactly one `started` exists; when it does and a `session_opened` exists, that
`started` has a monotonic time no later than the first `session_opened` that has
one; and `assert_no_flash_stream` passes over every halmasuit payload in timeline
order. A halmasuit restart across the swap is caught by the `started` count and
order checks.

Profile `ec2-greeter` checks an EC2 lab boot. Its run is journal-only, so every
time is null and events are ordered by monotonic time. P is the payload `pid` of
the one `started` event, and M is the first `drm_master_acquired` phase with a
monotonic time. The checks, one message each when they fail: exactly one
`started` exists and carries a pid; a `foreground_changed` to greeter exists; a
`frame_rendered` from P exists; a `GL Vendor` line from P exists; every `GL
Vendor` line names NVIDIA; M exists; a `lab-vkms-crc-initrd.service` start
exists; at least one vkms sample is earlier than M; no vkms sample earlier than M
is active; at least one active vkms sample follows the first `frame_rendered` from
P. A check that needs P, M, or that first frame is not evaluated when that anchor
is missing: the check that defines the anchor reports it.

Recorder index. `index.jsonl` is the frame recorder's index, one JSON object per
presented frame and output, with `seq`, `output`, `mono_us`, `width`, `height`,
`format`, and `dropped`. `read_recorder_index` returns its lines in file order and
rejects a malformed line by its line number. `recorder_failures` checks an index
against an ec2-greeter timeline; it is not a profile, and its messages are returned
the way a profile returns its own. The first frame is the earliest frame_rendered from
P, and the index must start no later than it. The boot-to-greeter window is the index
lines before the session foreground, or every line when there is none. In that window
every line's seq is the frame_id of a frame_rendered from P, the seqs strictly increase
per output in file order, no (output, seq) pair repeats, and every line has `dropped`
0. A seq absent between the window's lowest and highest seq is accepted only when the
timeline holds a frame_rendered and a frame_superseded with that frame_id, each from P.
Each output's video frame count equals its index lines, and each output has a frame
in the greeter-foreground window. `greeter_frames` picks, per output, the latest index
line in that window; that is the greeter PNG a run names. Seqs are shared by every
output, so the absent-seq rule is over seqs, not per output.
"""

from __future__ import annotations

import argparse
import bisect
import importlib
import json
import re
import struct
import sys
from collections import Counter
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

Timeline = dict[str, Any]
Profile = Callable[[Timeline], list[str]]

# tests/lib holds the phash and no-flash assertions the bios profile reuses. It
# is not a package, so its modules are imported by path.
TESTS_LIB = Path(__file__).resolve().parents[1] / "lib"
sys.path.insert(0, str(TESTS_LIB))
phash_progression = importlib.import_module("phash_progression")
visual = importlib.import_module("visual")

FPDT_NAMES = (
    "firmware_start",
    "bootloader_load",
    "bootloader_launch",
    "exitbootservice_start",
    "exitbootservice_end",
)
BGRT_FIELDS = ("status", "type", "version", "xoffset", "yoffset")
LOADER_GUID = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
EFI_FRAMEBUFFER_TYPE = 0x70
GREETD_STARTED_MESSAGE_ID = "39f53479d3a045ac8e11786248231fbf"
GREETD_UNIT = "greetd.service"
HALMASUIT_UNIT = "halmasuit.service"
INITRD_SAMPLER_UNIT = "lab-crc-initrd.service"
CRC_VERBS = ("start", "sample", "error")
VKMS_IDENTIFIER = "lab-vkms-crc"
INITRD_VKMS_UNIT = "lab-vkms-crc-initrd.service"
HEX_CRC = re.compile(r"[0-9a-f]{8}")
GL_TARGET = "smithay::backend::renderer::gles"
GL_PREFIXES = ("GL Vendor: ", "GL Renderer: ", "GL Version: ")
GL_VENDOR_PREFIX = "GL Vendor: "
DISPLAY_PATTERN = re.compile(
    r"efifb|simpledrm|simple-framebuffer|fbcon|Console: switching|\[drm\]|nvidia-drm|nvidia-modeset|NVRM"
)
DRIVER_PATTERN = re.compile(r"nvidia-drm|nvidia-modeset|NVRM")
JOURNAL_NAME = "journal.json"
TIMELINE_NAME = "timeline.json"
RECORDER_INDEX_NAME = "index.jsonl"
RECORDER_INT_FIELDS = ("seq", "mono_us", "width", "height", "dropped")


@dataclass(frozen=True)
class JournalEntry:
    message: str | None
    identifier: str
    transport: str
    unit: str
    started_unit: str
    message_id: str
    boot_id: str
    mono_us: int | None
    pid: int | None


@dataclass(frozen=True)
class KernelZero:
    us: int | None
    source: str | None


def decode_message(raw: object) -> str | None:
    if isinstance(raw, str):
        return raw
    if isinstance(raw, list) and all(type(byte) is int and 0 <= byte <= 255 for byte in raw):
        return bytes(raw).decode("utf-8", errors="replace")
    return None


def monotonic_us(fields: dict[str, str]) -> int | None:
    raw = fields.get("_SOURCE_MONOTONIC_TIMESTAMP") or fields.get("__MONOTONIC_TIMESTAMP")
    return int(raw) if raw else None


def journal_pid(fields: dict[str, str]) -> int | None:
    raw = fields.get("_PID")
    return int(raw) if raw else None


def read_journal(path: Path) -> list[JournalEntry]:
    if not path.is_file():
        return []
    entries = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        fields = json.loads(line)
        entries.append(
            JournalEntry(
                message=decode_message(fields.get("MESSAGE")),
                identifier=fields.get("SYSLOG_IDENTIFIER", ""),
                transport=fields.get("_TRANSPORT", ""),
                unit=fields.get("_SYSTEMD_UNIT", ""),
                started_unit=fields.get("UNIT", ""),
                message_id=fields.get("MESSAGE_ID", ""),
                boot_id=fields.get("_BOOT_ID", ""),
                mono_us=monotonic_us(fields),
                pid=journal_pid(fields),
            )
        )
    return entries


def read_decimal(path: Path) -> int | None:
    return int(path.read_text().strip()) if path.is_file() else None


def read_fpdt(boot_dir: Path) -> dict[str, int] | None:
    values = {}
    for name in FPDT_NAMES:
        ns = read_decimal(boot_dir / f"{name}_ns")
        if ns is not None:
            values[f"{name}_us"] = ns // 1000
    return values or None


def read_efivar(efi_dir: Path, name: str) -> str | None:
    path = efi_dir / f"{name}-{LOADER_GUID}"
    if not path.is_file():
        return None
    return path.read_bytes()[4:].decode("utf-16-le").split("\x00", 1)[0]


def read_loader_time(efi_dir: Path, name: str) -> int | None:
    text = read_efivar(efi_dir, name)
    return int(text) if text is not None else None


def kernel_zero(fpdt: dict[str, int] | None, loader_exec_us: int | None) -> KernelZero:
    exit_end = (fpdt or {}).get("exitbootservice_end_us")
    if exit_end:
        return KernelZero(exit_end, "fpdt.exitbootservice_end")
    if loader_exec_us is not None:
        return KernelZero(loader_exec_us, "LoaderTimeExecUSec")
    return KernelZero(None, None)


def platform_us(zero: KernelZero, mono_us: int | None) -> int | None:
    if zero.us is None or mono_us is None:
        return None
    return zero.us + mono_us


def read_firmware_mode(path: Path) -> dict[str, Any] | None:
    if not path.is_file():
        return None
    data = path.read_bytes()
    if len(data) < 0x26:
        return None
    width, height, depth = struct.unpack_from("<HHH", data, 0x12)
    (linelength,) = struct.unpack_from("<H", data, 0x24)
    return {
        "width": width,
        "height": height,
        "depth": depth,
        "linelength": linelength,
        "efi": data[0x0F] == EFI_FRAMEBUFFER_TYPE,
    }


def read_logo(bgrt_dir: Path) -> dict[str, int] | None:
    logo: dict[str, int] = {}
    for name in BGRT_FIELDS:
        value = read_decimal(bgrt_dir / name)
        if value is None:
            return None
        logo[name] = value
    image = bgrt_dir / "image"
    if not image.is_file():
        return None
    data = image.read_bytes()
    if len(data) < 26:
        return None
    width, height = struct.unpack_from("<ii", data, 18)
    return {**logo, "width": width, "height": abs(height)}


def crc_verb(message: str) -> str:
    words = message.split(maxsplit=1)
    return words[0] if words else ""


def crc_fields(message: str) -> tuple[str, dict[str, str]]:
    verb, *tokens = message.split()
    return verb, dict(token.split("=", 1) for token in tokens)


def gl_detail(message: str) -> str | None:
    try:
        outer = json.loads(message)
    except ValueError:
        return None
    if not isinstance(outer, dict) or outer.get("target") != GL_TARGET:
        return None
    fields = outer.get("fields")
    text = fields.get("message") if isinstance(fields, dict) else None
    return text if isinstance(text, str) and text.startswith(GL_PREFIXES) else None


def vkms_crc(value: str) -> str:
    if not HEX_CRC.fullmatch(value):
        raise ValueError(f"expected eight hex digits, got {value!r}")
    return value


def halmasuit_payload(message: str) -> dict[str, Any] | None:
    try:
        outer = json.loads(message)
    except ValueError:
        return None
    fields = outer.get("fields") if isinstance(outer, dict) else None
    inner = fields.get("json") if isinstance(fields, dict) else None
    if not isinstance(inner, str):
        return None
    try:
        event = json.loads(inner)
    except ValueError:
        return None
    return event if isinstance(event, dict) and "event" in event else None


def flag(value: str) -> bool:
    if value not in ("0", "1"):
        raise ValueError(f"expected 0 or 1, got {value!r}")
    return value == "1"


def make_event(
    t_us: int | None, mono_us: int | None, kind: str, source: str, detail: str
) -> dict[str, Any]:
    return {"t_us": t_us, "mono_us": mono_us, "kind": kind, "source": source, "detail": detail}


def build_timeline(run: Path) -> Timeline:
    firmware_dir = run / "firmware"
    efi_dir = firmware_dir / "efi" / "efivars"
    fpdt = read_fpdt(firmware_dir / "acpi" / "fpdt" / "boot")
    loader_init_us = read_loader_time(efi_dir, "LoaderTimeInitUSec")
    loader_exec_us = read_loader_time(efi_dir, "LoaderTimeExecUSec")
    zero = kernel_zero(fpdt, loader_exec_us)
    entries = read_journal(run / JOURNAL_NAME)

    events: list[dict[str, Any]] = []
    for name, us in (fpdt or {}).items():
        events.append(make_event(us, None, "firmware", "fpdt", name))
    for name, us in (("LoaderTimeInitUSec", loader_init_us), ("LoaderTimeExecUSec", loader_exec_us)):
        if us is not None:
            events.append(make_event(us, None, "loader", "efivar", name))

    heads: dict[str, dict[str, list[dict[str, Any]]]] = {}
    sampler_starts: list[dict[str, Any]] = []
    probe: list[dict[str, Any]] = []
    greetd: JournalEntry | None = None

    for entry in entries:
        mono = entry.mono_us
        if greetd is None and entry.message_id == GREETD_STARTED_MESSAGE_ID and entry.started_unit == GREETD_UNIT:
            greetd = entry
            t = platform_us(zero, mono)
            events.append(make_event(t, mono, "unit", entry.identifier, "greetd.service started"))
        if entry.message is None:
            continue
        if entry.unit == HALMASUIT_UNIT:
            payload = halmasuit_payload(entry.message)
            if payload is not None:
                event = make_event(
                    platform_us(zero, mono), mono, "halmasuit", HALMASUIT_UNIT, payload["event"]
                )
                events.append({**event, "payload": payload, "pid": entry.pid})
            else:
                detail = gl_detail(entry.message)
                if detail is not None:
                    gl = make_event(platform_us(zero, mono), mono, "gl", HALMASUIT_UNIT, detail)
                    events.append({**gl, "pid": entry.pid})
        if entry.transport == "kernel" and DISPLAY_PATTERN.search(entry.message):
            kind = "driver-display" if DRIVER_PATTERN.search(entry.message) else "kernel-display"
            events.append(make_event(platform_us(zero, mono), mono, kind, "kernel", entry.message))
        elif entry.identifier == "lab-crc" and crc_verb(entry.message) in CRC_VERBS:
            verb, fields = crc_fields(entry.message)
            if verb == "start":
                mono = int(fields["t"])
                t = platform_us(zero, mono)
                sampler_starts.append({"t_us": t, "mono_us": mono, "unit": entry.unit})
                events.append(make_event(t, mono, "sampler-start", entry.unit, entry.message))
            elif verb in ("sample", "error"):
                mono = int(fields["t"])
                t = platform_us(zero, mono)
                head = heads.setdefault(
                    f"{fields['card']}/crtc{fields['crtc']}", {"samples": [], "errors": []}
                )
                if verb == "sample":
                    head["samples"].append(
                        {
                            "t_us": t,
                            "mono_us": mono,
                            "unit": entry.unit,
                            "comp": fields["comp"],
                            "rg": fields["rg"],
                            "out": fields["out"],
                            "comp_sup": flag(fields["comp_sup"]),
                            "rg_sup": flag(fields["rg_sup"]),
                            "out_sup": flag(fields["out_sup"]),
                        }
                    )
                else:
                    head["errors"].append(
                        {"t_us": t, "mono_us": mono, "unit": entry.unit, "errno": int(fields["errno"])}
                    )
                events.append(make_event(t, mono, "crc", entry.unit, entry.message))
        elif entry.identifier == "lab-crc":
            events.append(make_event(platform_us(zero, mono), mono, "crc", entry.unit, entry.message))
        elif entry.identifier == VKMS_IDENTIFIER and crc_verb(entry.message) in CRC_VERBS:
            verb, fields = crc_fields(entry.message)
            mono = int(fields["t"])
            t = platform_us(zero, mono)
            if verb == "start":
                events.append(make_event(t, mono, "vkms-sampler-start", entry.unit, entry.message))
            else:
                head = f"{fields['card']}/crtc{fields['crtc']}"
                if verb == "sample":
                    extra = {
                        "head": head,
                        "crc": vkms_crc(fields["crc"]),
                        "active": flag(fields["active"]),
                    }
                else:
                    extra = {"head": head, "errno": int(fields["errno"])}
                events.append({**make_event(t, mono, "vkms-crc", entry.unit, entry.message), **extra})
        elif entry.identifier == VKMS_IDENTIFIER:
            events.append(make_event(platform_us(zero, mono), mono, "vkms-crc", entry.unit, entry.message))
        elif entry.identifier == "lab-probe":
            t = platform_us(zero, mono)
            probe.append({"t_us": t, "mono_us": mono, "line": entry.message})
            events.append(make_event(t, mono, "probe", entry.identifier, entry.message))

    greetd_started = None
    greeter: dict[str, dict[str, int | None]] = {}
    if greetd is not None:
        greetd_mono = greetd.mono_us
        greetd_started = {"t_us": platform_us(zero, greetd_mono), "mono_us": greetd_mono}
        if greetd_mono is not None:
            for key, head in heads.items():
                after = [sample for sample in head["samples"] if sample["mono_us"] > greetd_mono]
                if after:
                    first = min(after, key=lambda sample: sample["mono_us"])
                    greeter[key] = {"t_us": first["t_us"], "mono_us": first["mono_us"]}
                    events.append(make_event(first["t_us"], first["mono_us"], "greeter", key, "first sample"))

    events.sort(key=lambda event: (event["t_us"] is None, event["t_us"] or 0, event["mono_us"] or 0))
    boot_id = next((entry.boot_id for entry in entries if entry.boot_id), None)
    return {
        "run": run.name,
        "boot_id": boot_id,
        "clock": {
            "zero": "cpu-reset",
            "kernel_zero_us": zero.us,
            "kernel_zero_source": zero.source,
        },
        "firmware": {
            "info": read_efivar(efi_dir, "LoaderFirmwareInfo"),
            "loader_init_us": loader_init_us,
            "loader_exec_us": loader_exec_us,
            "fpdt": fpdt,
        },
        "firmware_mode": read_firmware_mode(firmware_dir / "boot_params" / "data"),
        "logo": read_logo(firmware_dir / "acpi" / "bgrt"),
        "events": events,
        "heads": heads,
        "sampler_starts": sampler_starts,
        "greetd_started": greetd_started,
        "greeter_first_appearance": greeter,
        "probe": probe,
    }


def baseline_greetd(timeline: Timeline) -> list[str]:
    failures: list[str] = []
    firmware = timeline["firmware"]
    fpdt = firmware["fpdt"] or {}
    if not all(f"{name}_us" in fpdt for name in FPDT_NAMES):
        failures.append("FPDT boot times missing")
    if firmware["loader_init_us"] is None:
        failures.append("LoaderTimeInitUSec missing")
    if firmware["loader_exec_us"] is None:
        failures.append("LoaderTimeExecUSec missing")
    if timeline["firmware_mode"] is None:
        failures.append("firmware mode missing")
    if timeline["logo"] is None:
        failures.append("BGRT logo missing")
    kinds = {event["kind"] for event in timeline["events"]}
    if "kernel-display" not in kinds:
        failures.append("no kernel-display event")
    if "driver-display" not in kinds:
        failures.append("no driver-display event")
    if not any(head["samples"] for head in timeline["heads"].values()):
        failures.append("no sample")
    if not any(start["unit"] == INITRD_SAMPLER_UNIT for start in timeline["sampler_starts"]):
        failures.append(f"no lab-crc start from {INITRD_SAMPLER_UNIT}")
    if timeline["greetd_started"] is None:
        failures.append("greetd start missing")
    if not timeline["greeter_first_appearance"]:
        failures.append("no greeter first appearance")
    return failures


ZERO_CHECKSUM = "00000000"
DRM_MASTER_PHASE = "drm_master_acquired"
DISPLAY_KINDS = ("kernel-display", "driver-display")


def payload_matches(payload: dict[str, Any], name: str, **fields: Any) -> bool:
    return payload.get("event") == name and all(payload.get(key) == value for key, value in fields.items())


def foreground_window(timed: list[dict[str, Any]]) -> tuple[int | None, int | None]:
    """The greeter's first foreground time and the next session foreground time.

    The end is None while no session foreground follows the greeter's.
    """
    start = None
    for event in timed:
        payload = event["payload"]
        if payload.get("event") != "foreground_changed":
            continue
        if start is None and payload.get("to") == "greeter":
            start = event["mono_us"]
        elif start is not None and payload.get("to") == "session":
            return start, event["mono_us"]
    return start, None


def drm_master_check(timed: list[dict[str, Any]]) -> tuple[list[str], int | None]:
    master = next(
        (event for event in timed if payload_matches(event["payload"], "phase_entered", phase=DRM_MASTER_PHASE)),
        None,
    )
    if master is None:
        return ["drm_master_acquired missing"], None
    return [], master["mono_us"]


def display_before_master_failures(display: list[dict[str, Any]], m: int | None) -> list[str]:
    if m is None:
        return []
    before_master = [event for event in display if event["mono_us"] is None or event["mono_us"] < m]
    failures: list[str] = []
    if any("Console: switching" in event["detail"] for event in before_master):
        failures.append("fbcon console switched before drm_master_acquired")
    if any("is primary device" in event["detail"] and "fbcon" in event["detail"] for event in before_master):
        failures.append("fbcon claimed the primary device before drm_master_acquired")
    if any(
        DRIVER_PATTERN.search(event["detail"]) and "frame buffer device" in event["detail"]
        for event in before_master
    ):
        failures.append("NVIDIA drm frame buffer device line before drm_master_acquired")
    return failures


def active_head_failures(heads: dict[str, Any], m: int | None) -> list[str]:
    if m is None:
        return []
    failures: list[str] = []
    active = {key: [sample for sample in head["samples"] if sample["mono_us"] > m] for key, head in heads.items()}
    active = {key: samples for key, samples in active.items() if samples}
    if not active:
        failures.append("no active head after drm_master_acquired")
    zero_only = [key for key, samples in active.items() if all(s["comp"] == ZERO_CHECKSUM for s in samples)]
    if zero_only:
        failures.append(f"no nonzero checksum after drm_master_acquired on: {', '.join(zero_only)}")
    constant = [key for key, samples in active.items() if len({s["comp"] for s in samples}) < 2]
    if constant:
        failures.append(f"checksum takes one value after drm_master_acquired on: {', '.join(constant)}")
    return failures


def client_and_foreground_failures(payloads: list[dict[str, Any]]) -> list[str]:
    failures: list[str] = []
    if not any(payload_matches(payload, "client_first_frame", role="overlay") for payload in payloads):
        failures.append("no client_first_frame{role: overlay}")
    if not any(payload_matches(payload, "foreground_changed", to="greeter") for payload in payloads):
        failures.append("no foreground_changed{to: greeter}")
    return failures


def from_pid(event: dict[str, Any], pid: int | None) -> bool:
    return pid is not None and event["pid"] == pid


def ec2_started(halmasuit: list[dict[str, Any]]) -> tuple[list[str], int | None]:
    started = [event for event in halmasuit if payload_matches(event["payload"], "started")]
    if len(started) != 1:
        return [f"{len(started)} halmasuit started events; expected exactly one"], None
    pid = started[0]["payload"].get("pid")
    if type(pid) is not int:
        return ["started event carries no pid"], None
    return [], pid


def ec2_first_frame(timed: list[dict[str, Any]], pid: int | None) -> dict[str, Any] | None:
    frames = [event for event in timed if payload_matches(event["payload"], "frame_rendered") and from_pid(event, pid)]
    return min(frames, key=lambda event: event["mono_us"]) if frames else None


def ec2_greeter(timeline: Timeline) -> list[str]:
    events = timeline["events"]
    halmasuit = [event for event in events if event["kind"] == "halmasuit"]
    timed = [event for event in halmasuit if event["mono_us"] is not None]
    payloads = [event["payload"] for event in halmasuit]
    vkms = [event for event in events if event["kind"] == "vkms-crc" and "active" in event]

    failures, pid = ec2_started(halmasuit)
    if not any(payload_matches(payload, "foreground_changed", to="greeter") for payload in payloads):
        failures.append("no foreground_changed{to: greeter}")

    first_frame = ec2_first_frame(timed, pid)
    if pid is not None and first_frame is None:
        failures.append(f"no frame_rendered from pid {pid}")

    gl_vendor = [event for event in events if event["kind"] == "gl" and event["detail"].startswith(GL_VENDOR_PREFIX)]
    if pid is not None and not any(from_pid(event, pid) for event in gl_vendor):
        failures.append(f"no GL Vendor line from pid {pid}")
    not_nvidia = [event["detail"] for event in gl_vendor if "NVIDIA" not in event["detail"]]
    if not_nvidia:
        failures.append(f"GL Vendor is not NVIDIA: {'; '.join(not_nvidia)}")

    master_failures, m = drm_master_check(timed)
    failures += master_failures
    if not any(event["kind"] == "vkms-sampler-start" and event["source"] == INITRD_VKMS_UNIT for event in events):
        failures.append(f"no lab-vkms-crc start from {INITRD_VKMS_UNIT}")
    if m is not None:
        before = [event for event in vkms if event["mono_us"] < m]
        if not before:
            failures.append("no vkms sample before drm_master_acquired")
        active = sorted({event["head"] for event in before if event["active"]})
        if active:
            failures.append(f"vkms sample active before drm_master_acquired on: {', '.join(active)}")
    if first_frame is not None and not any(
        event["active"] and event["mono_us"] > first_frame["mono_us"] for event in vkms
    ):
        failures.append(f"no active vkms sample after the first frame_rendered from pid {pid}")
    return failures


def recorder_frame(text: str, number: int) -> dict[str, Any]:
    where = f"{RECORDER_INDEX_NAME} line {number}"
    try:
        frame = json.loads(text)
    except ValueError as exc:
        raise ValueError(f"{where}: not JSON: {exc}") from exc
    if not isinstance(frame, dict):
        raise ValueError(f"{where}: not a JSON object")
    for field in RECORDER_INT_FIELDS:
        value = frame.get(field)
        if type(value) is not int or value < 0:
            raise ValueError(f"{where}: {field} is not a non-negative int")
    if not isinstance(frame.get("format"), str):
        raise ValueError(f"{where}: format is not a string")
    output = frame.get("output")
    if not isinstance(output, str) or output in ("", ".", "..") or "/" in output or "\0" in output:
        raise ValueError(f"{where}: output is not one path component")
    return frame


def read_recorder_index(directory: Path) -> list[dict[str, Any]]:
    with (directory / RECORDER_INDEX_NAME).open(encoding="utf-8") as handle:
        return [recorder_frame(text, number) for number, text in enumerate(handle, start=1)]


def timed_halmasuit(timeline: Timeline) -> list[dict[str, Any]]:
    return [event for event in timeline["events"] if event["kind"] == "halmasuit" and event["mono_us"] is not None]


def greeter_frames(timeline: Timeline, frames: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Each output's index line with the greatest mono_us in the greeter-foreground window.

    The window starts at the greeter's foreground time, inclusive, and ends at the
    session foreground time, exclusive. The result is empty without a greeter foreground.
    """
    start, end = foreground_window(timed_halmasuit(timeline))
    if start is None:
        return {}
    picked: dict[str, dict[str, Any]] = {}
    for frame in frames:
        mono = frame["mono_us"]
        if mono < start or (end is not None and mono >= end):
            continue
        held = picked.get(frame["output"])
        if held is None or mono >= held["mono_us"]:
            picked[frame["output"]] = frame
    return picked


def frame_ids(timed: list[dict[str, Any]], name: str, pid: int | None) -> set[int]:
    ids: set[int] = set()
    for event in timed:
        frame_id = event["payload"].get("frame_id")
        if from_pid(event, pid) and payload_matches(event["payload"], name) and type(frame_id) is int:
            ids.add(frame_id)
    return ids


def unexcused_gaps(seqs: list[int], excused: list[int]) -> list[tuple[int, int]]:
    """The inclusive runs of absent seqs between consecutive seqs, leaving out excused seqs.

    `seqs` and `excused` are sorted, and `seqs` holds no duplicates.
    """
    runs: list[tuple[int, int]] = []
    for low, high in zip(seqs, seqs[1:]):
        cursor = low + 1
        for seq in excused[bisect.bisect_right(excused, low) : bisect.bisect_left(excused, high)]:
            if seq > cursor:
                runs.append((cursor, seq - 1))
            cursor = seq + 1
        if cursor < high:
            runs.append((cursor, high - 1))
    return runs


def out_of_order(boot: list[dict[str, Any]], repeats: set[tuple[str, int]]) -> list[tuple[str, int, int]]:
    """Lines whose seq does not exceed the previous seq of their output; a repeated pair is left to its own check."""
    last: dict[str, int] = {}
    stepped: list[tuple[str, int, int]] = []
    for frame in boot:
        output, seq = frame["output"], frame["seq"]
        previous = last.get(output)
        if previous is not None and seq <= previous and (output, seq) not in repeats:
            stepped.append((output, seq, previous))
        last[output] = seq
    return stepped


def recorder_boot_failures(boot: list[dict[str, Any]], timed: list[dict[str, Any]], pid: int | None) -> list[str]:
    failures: list[str] = []
    seqs = sorted({frame["seq"] for frame in boot})
    rendered = frame_ids(timed, "frame_rendered", pid)
    superseded = frame_ids(timed, "frame_superseded", pid)
    gaps = unexcused_gaps(seqs, sorted(rendered & superseded))
    pairs = Counter((frame["output"], frame["seq"]) for frame in boot)
    repeated = sorted(pair for pair, count in pairs.items() if count > 1)
    unrendered = [frame for frame in boot if frame["seq"] not in rendered]
    stepped = out_of_order(boot, set(repeated))
    parts: list[str] = []
    if gaps:
        low, high = gaps[0]
        missing = f"seq {low} missing" if low == high else f"seqs {low}..{high} missing"
        parts.append(missing)
    if unrendered:
        first = unrendered[0]
        parts.append(
            f"{len(unrendered)} line(s) with no frame_rendered from the started pid, "
            f"first {first['output']} seq {first['seq']}"
        )
    if stepped:
        output, seq, previous = stepped[0]
        parts.append(
            f"{len(stepped)} line(s) with seq not increasing per output, "
            f"first {output} seq {seq} after seq {previous}"
        )
    if repeated:
        output, seq = repeated[0]
        parts.append(f"{len(repeated)} (output, seq) pair(s) repeated, first {output} seq {seq}")
    if parts:
        failures.append("boot-to-greeter frames: " + "; ".join(parts))
    dropped = Counter(frame["output"] for frame in boot if frame["dropped"] != 0)
    if dropped:
        listed = ", ".join(f"{output} ({count} line(s))" for output, count in sorted(dropped.items()))
        failures.append(f"dropped frames in boot-to-greeter window: {listed}")
    return failures


def recorder_count_failures(frames: list[dict[str, Any]], video_frames: Mapping[str, int]) -> list[str]:
    counts = Counter(frame["output"] for frame in frames)
    mismatched: list[str] = []
    for output in sorted(counts):
        lines = counts[output]
        if output not in video_frames:
            mismatched.append(f"{output} has no video frame count for {lines} index line(s)")
        elif video_frames[output] != lines:
            mismatched.append(f"{output} has {video_frames[output]} video frame(s) for {lines} index line(s)")
    if not mismatched:
        return []
    return [f"video frame count differs from index: {'; '.join(mismatched)}"]


def recorder_failures(
    timeline: Timeline, frames: list[dict[str, Any]], video_frames: Mapping[str, int]
) -> list[str]:
    if not frames:
        return ["recorder index holds no frames"]
    halmasuit = [event for event in timeline["events"] if event["kind"] == "halmasuit"]
    timed = timed_halmasuit(timeline)
    _, pid = ec2_started(halmasuit)
    first = ec2_first_frame(timed, pid)
    failures: list[str] = []

    earliest = min(frame["mono_us"] for frame in frames)
    if first is not None and earliest > first["mono_us"]:
        failures.append(
            f"recorder index starts at {earliest} us, after the first frame_rendered "
            f"from pid {pid} at {first['mono_us']} us"
        )
    start, end = foreground_window(timed)
    boot = [frame for frame in frames if end is None or frame["mono_us"] < end]
    failures += recorder_boot_failures(boot, timed, pid)
    failures += recorder_count_failures(frames, video_frames)
    if start is not None:
        seen = greeter_frames(timeline, frames)
        missing = sorted({frame["output"] for frame in frames} - seen.keys())
        if missing:
            failures.append(f"no frame in the greeter foreground window for output(s): {', '.join(missing)}")
    return failures


def bios_shader_greeter(timeline: Timeline) -> list[str]:
    events = timeline["events"]
    halmasuit = [event for event in events if event["kind"] == "halmasuit"]
    timed = [event for event in halmasuit if event["mono_us"] is not None]
    payloads = [event["payload"] for event in halmasuit]
    display = [event for event in events if event["kind"] in DISPLAY_KINDS]
    heads = timeline["heads"]

    failures, m = drm_master_check(timed)
    failures += display_before_master_failures(display, m)

    if timeline["firmware_mode"] is None:
        failures.append("firmware mode missing")
    if timeline["logo"] is None:
        failures.append("BGRT logo missing")

    if not any(payload["event"] == "frame_rendered" and payload.get("degenerate") is False for payload in payloads):
        failures.append("no non-degenerate frame_rendered")
    try:
        phash_progression.assert_animating(payloads)
    except AssertionError as exc:
        failures.append(str(exc))

    failures += active_head_failures(heads, m)
    failures += client_and_foreground_failures(payloads)
    start, end = foreground_window(timed)
    if start is not None:
        zero_in_greeter = [
            key
            for key, head in heads.items()
            if any(
                s["comp"] == ZERO_CHECKSUM and s["mono_us"] >= start and (end is None or s["mono_us"] < end)
                for s in head["samples"]
            )
        ]
        if zero_in_greeter:
            failures.append(f"zero checksum during greeter foreground on: {', '.join(zero_in_greeter)}")

    try:
        visual.assert_no_flash_stream(None, events=payloads)
    except AssertionError as exc:
        failures.append(str(exc))

    first_frame = next((event for event in timed if payload_matches(event["payload"], "frame_rendered")), None)
    if first_frame is not None:
        frame_mono = first_frame["mono_us"]
        zero_after_frame = [
            key
            for key, head in heads.items()
            if any(s["comp"] == ZERO_CHECKSUM and s["mono_us"] > frame_mono for s in head["samples"])
        ]
        if zero_after_frame:
            failures.append(f"zero checksum after first frame_rendered on: {', '.join(zero_after_frame)}")
    return failures


def production(timeline: Timeline) -> list[str]:
    halmasuit = [event for event in timeline["events"] if event["kind"] == "halmasuit"]
    timed = [event for event in halmasuit if event["mono_us"] is not None]
    payloads = [event["payload"] for event in halmasuit]
    display = [event for event in timeline["events"] if event["kind"] in DISPLAY_KINDS]

    failures, m = drm_master_check(timed)
    failures += display_before_master_failures(display, m)
    failures += active_head_failures(timeline["heads"], m)
    failures += client_and_foreground_failures(payloads)
    return failures


def attended_login(timeline: Timeline) -> list[str]:
    halmasuit = [event for event in timeline["events"] if event["kind"] == "halmasuit"]
    payloads = [event["payload"] for event in halmasuit]
    opened = [event for event in halmasuit if payload_matches(event["payload"], "session_opened")]
    started = [event for event in halmasuit if payload_matches(event["payload"], "started")]
    failures: list[str] = []

    if not opened:
        failures.append("no session_opened")
    if not any(payload_matches(payload, "session_client_first_frame") for payload in payloads):
        failures.append("no session_client_first_frame")
    if len(started) != 1:
        failures.append(f"{len(started)} halmasuit started events; expected exactly one")
    opened_mono = next((event["mono_us"] for event in opened if event["mono_us"] is not None), None)
    if len(started) == 1 and opened_mono is not None:
        started_mono = started[0]["mono_us"]
        if started_mono is not None and started_mono > opened_mono:
            failures.append("halmasuit started after session_opened")

    try:
        visual.assert_no_flash_stream(None, events=payloads)
    except AssertionError as exc:
        failures.append(str(exc))
    return failures


PROFILES: dict[str, Profile] = {
    "attended-login": attended_login,
    "baseline-greetd": baseline_greetd,
    "bios-shader-greeter": bios_shader_greeter,
    "ec2-greeter": ec2_greeter,
    "production": production,
}


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Build timeline.json for one lab run and check a profile.")
    parser.add_argument("run", type=Path, metavar="RUN", help="lab run directory")
    parser.add_argument(
        "--expect",
        default="baseline-greetd",
        metavar="PROFILE",
        help=f"profile to check (default: baseline-greetd; known: {', '.join(sorted(PROFILES))})",
    )
    args = parser.parse_args(argv)
    profile = PROFILES.get(args.expect)
    if profile is None:
        print(f"timeline: unknown profile {args.expect!r}; known: {', '.join(sorted(PROFILES))}", file=sys.stderr)
        return 2
    if not args.run.is_dir():
        print(f"timeline: no such run directory: {args.run}", file=sys.stderr)
        return 2
    try:
        timeline = build_timeline(args.run)
    except (KeyError, OSError, ValueError) as exc:
        print(f"timeline: {args.run}: malformed run: {exc!r}", file=sys.stderr)
        return 2
    (args.run / TIMELINE_NAME).write_text(json.dumps(timeline, indent=2) + "\n")
    failures = profile(timeline)
    for failure in failures:
        print(f"FAIL {args.expect}: {failure}")
    if failures:
        return 1
    print(f"PASS {args.expect}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
