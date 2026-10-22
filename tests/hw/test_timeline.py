#!/usr/bin/env python3
"""Selftest for tests/hw/timeline.py.

Each run directory is written into a temporary directory by this file, in the
layout the lab runner collects. The values are synthetic: no machine id, MAC
address, IP address, or user name appears in them.
"""

import contextlib
import io
import json
import struct
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path

import timeline

BOOT_ID = "0f1e2d3c4b5a69788796a5b4c3d2e1f0"
GREETD_MESSAGE_ID = "39f53479d3a045ac8e11786248231fbf"
LOADER_GUID = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
FPDT = "firmware/acpi/fpdt/boot"
EFIVARS = "firmware/efi/efivars"
BGRT = "firmware/acpi/bgrt"
BOOT_PARAMS = "firmware/boot_params/data"
JOURNAL = "journal.json"
INITRD_UNIT = "lab-crc-initrd.service"
ROOTFS_UNIT = "lab-crc.service"
FATAL_ERRNO = "lab-crc: nvidia card fd lost: No such device"
FATAL_BARE = "lab-crc: nvidia card fd lost"
TIMELINE_KEYS = {
    "run",
    "boot_id",
    "clock",
    "firmware",
    "firmware_mode",
    "logo",
    "events",
    "heads",
    "sampler_starts",
    "greetd_started",
    "greeter_first_appearance",
    "probe",
}
CLOCK_KEYS = {"zero", "kernel_zero_us", "kernel_zero_source"}

# FPDT exitbootservice_end_ns is 3_250_123_456 ns, which floors to 3_250_123 us.
KERNEL_ZERO_FPDT_US = 3_250_123
# LoaderTimeExecUSec, the anchor used when exitbootservice_end is absent or zero.
KERNEL_ZERO_LOADER_US = 3_400_000
GREETD_MONO_US = 2_000_000


def journal_line(
    message,
    *,
    identifier="",
    transport="",
    mono=None,
    source=None,
    unit="",
    started_unit="",
    message_id="",
    as_bytes=False,
):
    fields = {
        "MESSAGE": list(message.encode()) if as_bytes else message,
        "SYSLOG_IDENTIFIER": identifier,
        "_TRANSPORT": transport,
        "_BOOT_ID": BOOT_ID,
    }
    if mono is not None:
        fields["__MONOTONIC_TIMESTAMP"] = str(mono)
    if source is not None:
        fields["_SOURCE_MONOTONIC_TIMESTAMP"] = str(source)
    if unit:
        fields["_SYSTEMD_UNIT"] = unit
    if started_unit:
        fields["UNIT"] = started_unit
    if message_id:
        fields["MESSAGE_ID"] = message_id
    return json.dumps(fields)


def crc_sample(card, crtc, t_us, unit, *, as_bytes=False):
    message = (
        f"sample card={card} crtc={crtc} t={t_us} comp=00000001 rg=00000002 "
        "out=00000003 comp_sup=1 rg_sup=1 out_sup=0"
    )
    return journal_line(message, identifier="lab-crc", unit=unit, as_bytes=as_bytes)


def crc_error(card, crtc, t_us, errno, unit):
    message = f"error card={card} crtc={crtc} t={t_us} errno={errno}"
    return journal_line(message, identifier="lab-crc", unit=unit)


def complete_journal():
    return [
        journal_line(
            "efifb: framebuffer at 0xd0000000",
            identifier="kernel",
            transport="kernel",
            mono=7,
            source=500_000,
        ),
        journal_line(
            "nvidia-drm: loading driver",
            identifier="kernel",
            transport="kernel",
            source=900_000,
        ),
        journal_line(
            "usb 1-1: new high-speed USB device",
            identifier="kernel",
            transport="kernel",
            source=950_000,
        ),
        journal_line("efifb: copied into the journal", identifier="lab", transport="journal", mono=10),
        journal_line("start t=1000000", identifier="lab-crc", unit=INITRD_UNIT, mono=5),
        crc_sample("card0", "42", 1_100_000, INITRD_UNIT),
        crc_error("card1", "7", 1_150_000, 19, INITRD_UNIT),
        crc_sample("card0", "42", 1_200_000, INITRD_UNIT),
        journal_line(
            "Started Greeter.",
            identifier="systemd",
            started_unit="greetd.service",
            message_id=GREETD_MESSAGE_ID,
            mono=GREETD_MONO_US,
        ),
        crc_sample("card1", "7", 2_000_000, ROOTFS_UNIT),
        crc_sample("card1", "7", 2_050_000, ROOTFS_UNIT),
        crc_sample("card0", "42", 2_100_000, ROOTFS_UNIT),
        journal_line("device node=/dev/dri/card0", identifier="lab-probe", mono=2_200_000),
    ]


def text(value):
    return value.encode()


def efivar(value):
    return b"\x07\x00\x00\x00" + value.encode("utf-16-le") + b"\x00\x00"


def bmp(width, height):
    header = bytearray(54)
    header[0:2] = b"BM"
    struct.pack_into("<ii", header, 18, width, height)
    return bytes(header)


def boot_params(*, efi):
    data = bytearray(0x30)
    data[0x0F] = 0x70 if efi else 0x23
    struct.pack_into("<HHH", data, 0x12, 1024, 768, 32)
    struct.pack_into("<H", data, 0x24, 4096)
    return bytes(data)


def journal_bytes(lines):
    return ("\n".join(lines) + "\n").encode()


def complete_files():
    return {
        f"{FPDT}/firmware_start_ns": text("1234567890"),
        f"{FPDT}/bootloader_load_ns": text("2000000000"),
        f"{FPDT}/bootloader_launch_ns": text("2500000000"),
        f"{FPDT}/exitbootservice_start_ns": text("3000000000"),
        f"{FPDT}/exitbootservice_end_ns": text("3250123456"),
        f"{EFIVARS}/LoaderTimeInitUSec-{LOADER_GUID}": efivar("1500000"),
        f"{EFIVARS}/LoaderTimeExecUSec-{LOADER_GUID}": efivar("3400000"),
        f"{EFIVARS}/LoaderFirmwareInfo-{LOADER_GUID}": efivar("EDK II 1.0"),
        f"{BGRT}/status": text("1"),
        f"{BGRT}/type": text("0"),
        f"{BGRT}/version": text("1"),
        f"{BGRT}/xoffset": text("100"),
        f"{BGRT}/yoffset": text("200"),
        f"{BGRT}/image": bmp(1024, -768),
        BOOT_PARAMS: boot_params(efi=True),
        JOURNAL: journal_bytes(complete_journal()),
    }


def without(files, *paths):
    return {path: data for path, data in files.items() if path not in paths}


def journal_of(files):
    return files[JOURNAL].decode().splitlines()


def with_journal(files, lines):
    return {**files, JOURNAL: journal_bytes(lines)}


def without_lines(files, marker):
    return with_journal(files, [line for line in journal_of(files) if marker not in line])


def replacing_lines(files, marker, replacement):
    return with_journal(files, [replacement if marker in line else line for line in journal_of(files)])


def inserting_after(files, marker, *lines):
    existing = journal_of(files)
    index = next(i for i, line in enumerate(existing) if marker in line)
    return with_journal(files, existing[: index + 1] + list(lines) + existing[index + 1 :])


def sampler_fatal(message, mono):
    return journal_line(message, identifier="lab-crc", transport="stdout", unit=ROOTFS_UNIT, mono=mono)


def write_run(root, files):
    run = root / "run"
    for rel, data in files.items():
        target = run / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    return run


def build(files=None):
    files = complete_files() if files is None else files
    with tempfile.TemporaryDirectory() as tmp:
        return timeline.build_timeline(write_run(Path(tmp), files))


def one_event(doc, detail):
    matches = [event for event in doc["events"] if event["detail"] == detail]
    if len(matches) != 1:
        raise AssertionError(f"expected one event with detail {detail!r}, got {len(matches)}")
    return matches[0]


def run_cli(argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = timeline.main(argv)
    return code, out.getvalue(), err.getvalue()


class HeaderTest(unittest.TestCase):
    def test_document_has_frozen_keys_and_identity(self):
        doc = build()
        self.assertEqual(set(doc), TIMELINE_KEYS)
        self.assertEqual(set(doc["clock"]), CLOCK_KEYS)
        self.assertEqual(doc["run"], "run")
        self.assertEqual(doc["boot_id"], BOOT_ID)


class AnchorTest(unittest.TestCase):
    def test_kernel_zero_is_fpdt_exit_boot_services_end(self):
        doc = build()
        self.assertEqual(
            doc["clock"],
            {
                "zero": "cpu-reset",
                "kernel_zero_us": KERNEL_ZERO_FPDT_US,
                "kernel_zero_source": "fpdt.exitbootservice_end",
            },
        )
        efifb = one_event(doc, "efifb: framebuffer at 0xd0000000")
        self.assertEqual(efifb["kind"], "kernel-display")
        self.assertEqual(efifb["t_us"], KERNEL_ZERO_FPDT_US + 500_000)
        self.assertEqual(efifb["mono_us"], 500_000)

    def test_kernel_zero_falls_back_to_loader_exec(self):
        cases = {
            "exitbootservice_end zero": {**complete_files(), f"{FPDT}/exitbootservice_end_ns": text("0")},
            "exitbootservice_end absent": without(complete_files(), f"{FPDT}/exitbootservice_end_ns"),
        }
        for label, files in cases.items():
            with self.subTest(label):
                doc = build(files)
                self.assertEqual(
                    doc["clock"],
                    {
                        "zero": "cpu-reset",
                        "kernel_zero_us": KERNEL_ZERO_LOADER_US,
                        "kernel_zero_source": "LoaderTimeExecUSec",
                    },
                )
                efifb = one_event(doc, "efifb: framebuffer at 0xd0000000")
                self.assertEqual(efifb["t_us"], KERNEL_ZERO_LOADER_US + 500_000)

    def test_times_stay_null_without_an_anchor(self):
        files = without(
            complete_files(),
            f"{FPDT}/exitbootservice_end_ns",
            f"{EFIVARS}/LoaderTimeExecUSec-{LOADER_GUID}",
        )
        doc = build(files)
        self.assertEqual(
            doc["clock"],
            {"zero": "cpu-reset", "kernel_zero_us": None, "kernel_zero_source": None},
        )
        self.assertEqual(doc["greetd_started"], {"t_us": None, "mono_us": GREETD_MONO_US})
        for event in doc["events"]:
            if event["mono_us"] is not None:
                self.assertIsNone(event["t_us"], event)

    def test_kmsg_source_time_wins_over_journal_timestamp(self):
        doc = build()
        efifb = one_event(doc, "efifb: framebuffer at 0xd0000000")
        self.assertEqual(efifb["mono_us"], 500_000)

    def test_lab_crc_time_wins_over_journal_timestamp(self):
        doc = build()
        self.assertEqual(
            doc["sampler_starts"],
            [{"t_us": KERNEL_ZERO_FPDT_US + 1_000_000, "mono_us": 1_000_000, "unit": INITRD_UNIT}],
        )

    def test_byte_list_messages_are_decoded(self):
        files = replacing_lines(
            complete_files(),
            "efifb: framebuffer",
            journal_line(
                "efifb: framebuffer at 0xd0000000",
                identifier="kernel",
                transport="kernel",
                mono=7,
                source=500_000,
                as_bytes=True,
            ),
        )
        files = replacing_lines(
            files,
            "t=2050000",
            crc_sample("card1", "7", 2_050_000, ROOTFS_UNIT, as_bytes=True),
        )
        doc = build(files)
        self.assertEqual(one_event(doc, "efifb: framebuffer at 0xd0000000")["kind"], "kernel-display")
        self.assertEqual(
            [sample["mono_us"] for sample in doc["heads"]["card1/crtc7"]["samples"]],
            [2_000_000, 2_050_000],
        )

    def test_events_sorted_by_time(self):
        doc = build()
        times = [event["t_us"] for event in doc["events"]]
        self.assertEqual(times, sorted(times))


class HeadTest(unittest.TestCase):
    def test_samples_and_errors_attributed_to_their_head(self):
        doc = build()
        self.assertEqual(set(doc["heads"]), {"card0/crtc42", "card1/crtc7"})
        card0 = doc["heads"]["card0/crtc42"]
        card1 = doc["heads"]["card1/crtc7"]
        self.assertEqual(
            card0["samples"][0],
            {
                "t_us": KERNEL_ZERO_FPDT_US + 1_100_000,
                "mono_us": 1_100_000,
                "unit": INITRD_UNIT,
                "comp": "00000001",
                "rg": "00000002",
                "out": "00000003",
                "comp_sup": True,
                "rg_sup": True,
                "out_sup": False,
            },
        )
        self.assertEqual(
            [sample["mono_us"] for sample in card0["samples"]],
            [1_100_000, 1_200_000, 2_100_000],
        )
        self.assertEqual(card0["errors"], [])
        self.assertEqual([sample["mono_us"] for sample in card1["samples"]], [2_000_000, 2_050_000])
        self.assertEqual(
            card1["errors"],
            [
                {
                    "t_us": KERNEL_ZERO_FPDT_US + 1_150_000,
                    "mono_us": 1_150_000,
                    "unit": INITRD_UNIT,
                    "errno": 19,
                }
            ],
        )

    def test_sampler_fatal_output_attributed_to_no_head(self):
        doc = build(
            inserting_after(
                complete_files(),
                "t=1200000",
                sampler_fatal(FATAL_ERRNO, 1_250_000),
                sampler_fatal(FATAL_BARE, 1_260_000),
            )
        )
        self.assertEqual(doc["heads"], build()["heads"])
        self.assertEqual(doc["greeter_first_appearance"], build()["greeter_first_appearance"])

    def test_greeter_first_appearance_is_first_sample_strictly_after_greetd(self):
        doc = build()
        self.assertEqual(
            doc["greeter_first_appearance"],
            {
                "card0/crtc42": {"t_us": KERNEL_ZERO_FPDT_US + 2_100_000, "mono_us": 2_100_000},
                "card1/crtc7": {"t_us": KERNEL_ZERO_FPDT_US + 2_050_000, "mono_us": 2_050_000},
            },
        )

    def test_greetd_start_needs_message_id_and_unit(self):
        decoys = [
            journal_line(
                "Started other.",
                identifier="systemd",
                started_unit="other.service",
                message_id=GREETD_MESSAGE_ID,
                mono=100,
            ),
            journal_line(
                "Started unrelated.",
                identifier="systemd",
                started_unit="greetd.service",
                message_id="ffffffffffffffffffffffffffffffff",
                mono=200,
            ),
        ]
        doc = build(with_journal(complete_files(), decoys + journal_of(complete_files())))
        self.assertEqual(doc["greetd_started"], {"t_us": KERNEL_ZERO_FPDT_US + GREETD_MONO_US, "mono_us": GREETD_MONO_US})

    def test_no_greeter_without_greetd_start(self):
        doc = build(without_lines(complete_files(), "greetd.service"))
        self.assertIsNone(doc["greetd_started"])
        self.assertEqual(doc["greeter_first_appearance"], {})


class EventTest(unittest.TestCase):
    def test_display_lines_are_classified_by_kind(self):
        doc = build()
        self.assertEqual(one_event(doc, "efifb: framebuffer at 0xd0000000")["kind"], "kernel-display")
        self.assertEqual(one_event(doc, "nvidia-drm: loading driver")["kind"], "driver-display")
        details = [event["detail"] for event in doc["events"]]
        self.assertFalse(any("usb" in detail for detail in details))
        self.assertNotIn("efifb: copied into the journal", details)

    def test_sampler_fatal_output_is_a_crc_event(self):
        doc = build(
            inserting_after(
                complete_files(),
                "t=1200000",
                sampler_fatal(FATAL_ERRNO, 1_250_000),
                sampler_fatal(FATAL_BARE, 1_260_000),
            )
        )
        for message, mono in ((FATAL_ERRNO, 1_250_000), (FATAL_BARE, 1_260_000)):
            with self.subTest(message):
                event = one_event(doc, message)
                self.assertEqual(event["kind"], "crc")
                self.assertEqual(event["source"], ROOTFS_UNIT)
                self.assertEqual(event["mono_us"], mono)
                self.assertEqual(event["t_us"], KERNEL_ZERO_FPDT_US + mono)

    def test_probe_lines_recorded(self):
        doc = build()
        self.assertEqual(
            doc["probe"],
            [
                {
                    "t_us": KERNEL_ZERO_FPDT_US + 2_200_000,
                    "mono_us": 2_200_000,
                    "line": "device node=/dev/dri/card0",
                }
            ],
        )
        self.assertEqual(one_event(doc, "device node=/dev/dri/card0")["kind"], "probe")

    def test_greetd_start_event(self):
        doc = build()
        event = one_event(doc, "greetd.service started")
        self.assertEqual(event["kind"], "unit")
        self.assertEqual(event["t_us"], KERNEL_ZERO_FPDT_US + GREETD_MONO_US)


class FirmwareTest(unittest.TestCase):
    def test_firmware_times(self):
        doc = build()
        self.assertEqual(
            doc["firmware"],
            {
                "info": "EDK II 1.0",
                "loader_init_us": 1_500_000,
                "loader_exec_us": 3_400_000,
                "fpdt": {
                    "firmware_start_us": 1_234_567,
                    "bootloader_load_us": 2_000_000,
                    "bootloader_launch_us": 2_500_000,
                    "exitbootservice_start_us": 3_000_000,
                    "exitbootservice_end_us": KERNEL_ZERO_FPDT_US,
                },
            },
        )

    def test_firmware_mode_from_boot_params(self):
        doc = build()
        self.assertEqual(
            doc["firmware_mode"],
            {"width": 1024, "height": 768, "depth": 32, "linelength": 4096, "efi": True},
        )
        doc = build({**complete_files(), BOOT_PARAMS: boot_params(efi=False)})
        self.assertFalse(doc["firmware_mode"]["efi"])

    def test_logo_from_bgrt_uses_absolute_height(self):
        doc = build()
        self.assertEqual(
            doc["logo"],
            {"status": 1, "type": 0, "version": 1, "xoffset": 100, "yoffset": 200, "width": 1024, "height": 768},
        )


class BaselineTest(unittest.TestCase):
    def test_complete_run_passes(self):
        profile = timeline.PROFILES["baseline-greetd"]
        self.assertEqual(profile(build()), [])

    def test_each_missing_element_fails(self):
        cases: list[tuple[str, Callable[[dict], dict], str]] = [
            (
                "FPDT",
                lambda f: without(f, f"{FPDT}/bootloader_launch_ns"),
                "FPDT boot times missing",
            ),
            (
                "LoaderTimeInitUSec",
                lambda f: without(f, f"{EFIVARS}/LoaderTimeInitUSec-{LOADER_GUID}"),
                "LoaderTimeInitUSec missing",
            ),
            (
                "LoaderTimeExecUSec",
                lambda f: without(f, f"{EFIVARS}/LoaderTimeExecUSec-{LOADER_GUID}"),
                "LoaderTimeExecUSec missing",
            ),
            ("firmware mode", lambda f: without(f, BOOT_PARAMS), "firmware mode missing"),
            ("BGRT logo", lambda f: without(f, f"{BGRT}/status"), "BGRT logo missing"),
            (
                "kernel-display",
                lambda f: without_lines(f, "efifb: framebuffer"),
                "no kernel-display event",
            ),
            (
                "driver-display",
                lambda f: without_lines(f, "nvidia-drm"),
                "no driver-display event",
            ),
            ("sample", lambda f: without_lines(f, "sample card="), "no sample"),
            (
                "initrd start",
                lambda f: replacing_lines(
                    f,
                    "start t=",
                    journal_line("start t=1000000", identifier="lab-crc", unit=ROOTFS_UNIT, mono=5),
                ),
                "no lab-crc start",
            ),
            ("greetd start", lambda f: without_lines(f, "greetd.service"), "greetd start missing"),
            (
                "greeter appearance",
                lambda f: without_lines(without_lines(f, "t=2050000"), "t=2100000"),
                "no greeter first appearance",
            ),
        ]
        for label, mutate, fragment in cases:
            with self.subTest(label):
                failures = timeline.PROFILES["baseline-greetd"](build(mutate(complete_files())))
                self.assertTrue(failures, label)
                self.assertTrue(any(fragment in failure for failure in failures), failures)


class CliTest(unittest.TestCase):
    def test_complete_run_exits_zero_and_writes_timeline(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = write_run(Path(tmp), complete_files())
            code, out, _ = run_cli([str(run)])
            self.assertEqual(code, 0, out)
            self.assertIn("PASS", out)
            written = json.loads((run / "timeline.json").read_text())
        self.assertEqual(set(written), TIMELINE_KEYS)
        self.assertEqual(set(written["clock"]), CLOCK_KEYS)

    def test_explicit_expect_baseline_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = write_run(Path(tmp), complete_files())
            code, _, _ = run_cli([str(run), "--expect", "baseline-greetd"])
        self.assertEqual(code, 0)

    def test_incomplete_run_exits_one_and_still_writes_timeline(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = write_run(Path(tmp), without(complete_files(), f"{BGRT}/status"))
            code, out, _ = run_cli([str(run)])
            self.assertEqual(code, 1)
            self.assertIn("BGRT logo missing", out)
            self.assertTrue((run / "timeline.json").is_file())

    def test_sampler_restart_after_fatal_output_still_builds(self):
        restart = journal_line("start t=1300000", identifier="lab-crc", unit=ROOTFS_UNIT, mono=5)
        files = inserting_after(complete_files(), "t=1200000", sampler_fatal(FATAL_ERRNO, 1_250_000), restart)
        with tempfile.TemporaryDirectory() as tmp:
            run = write_run(Path(tmp), files)
            code, out, _ = run_cli([str(run)])
            self.assertEqual(code, 0, out)
            self.assertIn("PASS", out)
            written = json.loads((run / "timeline.json").read_text())
        self.assertEqual(
            [start["unit"] for start in written["sampler_starts"]],
            [INITRD_UNIT, ROOTFS_UNIT],
        )

    def test_malformed_contract_line_exits_two(self):
        cases = {
            "sample token without =": journal_line(
                "sample card=card0 crtc=42 t=1200000 comp=00000001 rg=00000002 out=00000003 "
                "comp_sup=1 rg_sup=1 out_sup",
                identifier="lab-crc",
                unit=INITRD_UNIT,
            ),
            "sample without t": journal_line(
                "sample card=card0 crtc=42 comp=00000001 rg=00000002 out=00000003 "
                "comp_sup=1 rg_sup=1 out_sup=0",
                identifier="lab-crc",
                unit=INITRD_UNIT,
            ),
        }
        for label, line in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                run = write_run(Path(tmp), replacing_lines(complete_files(), "t=1200000", line))
                code, _, err = run_cli([str(run)])
                self.assertEqual(code, 2)
                self.assertIn("malformed run", err)

    def test_unknown_profile_exits_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = write_run(Path(tmp), complete_files())
            code, _, err = run_cli([str(run), "--expect", "nope"])
        self.assertEqual(code, 2)
        self.assertIn("nope", err)

    def test_missing_run_directory_exits_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, _, _ = run_cli([str(Path(tmp) / "absent")])
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
