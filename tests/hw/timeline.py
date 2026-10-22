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
A lab-probe line is a `probe` event.
A `halmasuit.service` line whose MESSAGE is a tracing JSON line carrying a
halmasuit Event is a `halmasuit` event: its detail is the Event's `event` tag,
its payload is the whole Event object, and its time is the line's monotonic
time on the kernel clock. Every such line is kept, repeats included. Any other
line from that unit is ignored, and lines from other units produce no
`halmasuit` event.
greetd's start is the first entry whose MESSAGE_ID is the unit-started ID and
whose `UNIT` is `greetd.service`; it is a `unit` event.

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
"""

from __future__ import annotations

import argparse
import importlib
import json
import re
import struct
import sys
from collections.abc import Callable, Sequence
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
DISPLAY_PATTERN = re.compile(
    r"efifb|simpledrm|simple-framebuffer|fbcon|Console: switching|\[drm\]|nvidia-drm|nvidia-modeset|NVRM"
)
DRIVER_PATTERN = re.compile(r"nvidia-drm|nvidia-modeset|NVRM")
JOURNAL_NAME = "journal.json"
TIMELINE_NAME = "timeline.json"


@dataclass(frozen=True)
class JournalEntry:
    message: str
    identifier: str
    transport: str
    unit: str
    started_unit: str
    message_id: str
    boot_id: str
    mono_us: int | None


@dataclass(frozen=True)
class KernelZero:
    us: int | None
    source: str | None


def decode_message(raw: str | list[int]) -> str:
    if isinstance(raw, list):
        return bytes(raw).decode("utf-8", errors="replace")
    return raw


def monotonic_us(fields: dict[str, str]) -> int | None:
    raw = fields.get("_SOURCE_MONOTONIC_TIMESTAMP") or fields.get("__MONOTONIC_TIMESTAMP")
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
                message=decode_message(fields.get("MESSAGE", "")),
                identifier=fields.get("SYSLOG_IDENTIFIER", ""),
                transport=fields.get("_TRANSPORT", ""),
                unit=fields.get("_SYSTEMD_UNIT", ""),
                started_unit=fields.get("UNIT", ""),
                message_id=fields.get("MESSAGE_ID", ""),
                boot_id=fields.get("_BOOT_ID", ""),
                mono_us=monotonic_us(fields),
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
        if entry.unit == HALMASUIT_UNIT:
            payload = halmasuit_payload(entry.message)
            if payload is not None:
                event = make_event(
                    platform_us(zero, mono), mono, "halmasuit", HALMASUIT_UNIT, payload["event"]
                )
                events.append({**event, "payload": payload})
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


PROFILES: dict[str, Profile] = {
    "baseline-greetd": baseline_greetd,
    "bios-shader-greeter": bios_shader_greeter,
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
