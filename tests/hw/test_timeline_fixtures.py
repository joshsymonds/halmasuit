#!/usr/bin/env python3
"""Replays every committed fixture run through timeline.py.

A fixture run is a directory under tests/hw/fixtures/ that holds
`expected-timeline.json`; its `profile` file names the profile that must pass.
Runs are read in place: build_timeline never writes into the fixture. The
synthetic-baseline, gnomon-baseline, and synthetic-bios-shader-greeter runs
also assert their story facts by hand, so a golden file regenerated from a
broken timeline still fails.
"""

import errno
import itertools
import json
import re
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

import timeline

FIXTURE_ROOT = Path(__file__).resolve().parent / "fixtures"
EXPECTED_NAME = "expected-timeline.json"
PROFILE_NAME = "profile"
JOURNAL_NAME = "journal.json"
BASELINE_RUN = FIXTURE_ROOT / "synthetic-baseline"
GNOMON_RUN = FIXTURE_ROOT / "gnomon-baseline"
BIOS_RUN = FIXTURE_ROOT / "synthetic-bios-shader-greeter"
# exitbootservice_end_ns is 2_583_541_217, which floors to 2_583_541 us.
KERNEL_ZERO_US = 2_583_541
# Recorded gnomon boot: exitbootservice_end_ns floors to 19_664_764 us.
GNOMON_KERNEL_ZERO_US = 19_664_764
INITRD_UNIT = "lab-crc-initrd.service"
ROOTFS_UNIT = "lab-crc.service"
HALMASUIT_UNIT = "halmasuit.service"
# journald stamps a stdout stream with the uid and gid of the process that connected it. The initrd unit
# has no User=, so halmasuit connects as root and drops privileges in-process after the pivot; its
# stream keeps uid and gid 0 for every line.
HALMASUIT_STREAM_ID = "0"
HALMASUIT_PID = "1184"
GREETER_PID = 1459
HEAD_CRTC81 = "card1/crtc81"
HEAD_CRTC91 = "card1/crtc91"
HEAD_CRTC205 = "card1/crtc205"
HEAD_CRTC494 = "card1/crtc494"
PANEL_HEADS = (HEAD_CRTC205, HEAD_CRTC494)
# Story times in microseconds on the kernel clock, which KERNEL_ZERO_US anchors.
INITRD_SAMPLER_START_MONO = 2_050_113
HALMASUIT_STARTED_MONO = 2_088_000
INITRAMFS_INIT_MONO = 2_090_000
DRM_MASTER_MONO = 2_240_000
INIT_MONO = 2_250_000
WAYLAND_READY_MONO = 2_252_000
WALLPAPER_MONO = 2_262_000
FIRST_FRAME_MONO = 2_270_000
SCANOUT_MONO = 2_275_000
ROOTFS_SAMPLER_START_MONO = 2_980_120
ROOTFS_READY_MONO = 2_985_000
GREETD_READY_MONO = 2_990_000
GREETER_SPAWNED_MONO = 2_995_000
GREETER_MONO = 2_996_000
DEPRIVILEGED_MONO = 2_998_000
OVERLAY_MONO = 3_010_000
HOLD_END_MONO = GREETER_MONO + 30_000_000
FRAME_INTERVAL_US = 500_000
FRAME_COUNT = 63
FRAME_PIXELS = 2560 * 1440
FRAME_SAMPLE_TOLERANCE_US = 250_000
# Heads have no valid mode before the first NVIDIA modeset, so the first tick after DRM master is the
# first sample. The initrd sampler ticks twice, then rootfs ticks every half second to the end of the hold.
SAMPLE_TICKS = (2_300_000, 2_800_000) + tuple(3_300_000 + 500_000 * k for k in range(61))
# lab-crc reads each head's time right after that head's ioctl, so head 494 is sampled after head 205.
PANEL_SAMPLE_DELAY_US = {HEAD_CRTC205: 1_200, HEAD_CRTC494: 2_400}
NVIDIA_LINES = (
    "NVRM: loading NVIDIA UNIX Open Kernel Module for x86_64  615.78.08  Release Build  (nixbld@)  ",
    "nvidia-modeset: Loading NVIDIA UNIX Open Kernel Mode Setting Driver for x86_64  615.78.08"
    "  Release Build  (nixbld@)  ",
    "[drm] [nvidia-drm] [GPU ID 0x00000100] Loading driver",
    "[drm] Initialized nvidia-drm 0.0.0 for 0000:01:00.0 on minor 1",
)
EXPECTED_HALMASUIT_STORY = [
    (HALMASUIT_STARTED_MONO, {"event": "started", "pid": 1184, "version": "0.1.0"}),
    (INITRAMFS_INIT_MONO, {"event": "phase_entered", "phase": "initramfs_init"}),
    (DRM_MASTER_MONO, {"event": "phase_entered", "phase": "drm_master_acquired"}),
    (INIT_MONO, {"event": "phase_entered", "phase": "init"}),
    (WAYLAND_READY_MONO, {"event": "phase_entered", "phase": "wayland_ready"}),
    (WALLPAPER_MONO, {"event": "client_first_frame", "role": "wallpaper"}),
    (SCANOUT_MONO, {"event": "phase_entered", "phase": "scanout_active"}),
    (ROOTFS_READY_MONO, {"event": "phase_entered", "phase": "rootfs_ready"}),
    (GREETD_READY_MONO, {"event": "phase_entered", "phase": "greetd_ready"}),
    (GREETER_SPAWNED_MONO, {"event": "greeter_spawned", "pid": GREETER_PID}),
    (GREETER_MONO, {"event": "foreground_changed", "to": "greeter"}),
    (DEPRIVILEGED_MONO, {"event": "phase_entered", "phase": "deprivileged"}),
    (OVERLAY_MONO, {"event": "client_first_frame", "role": "overlay"}),
]
SAMPLE_LINE = re.compile(
    r"sample card=card1 crtc=(205|494) t=\d+ comp=[0-9a-f]{8} rg=[0-9a-f]{8} out=[0-9a-f]{8}"
    r" comp_sup=1 rg_sup=1 out_sup=1"
)
EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)
COMMON_FIELDS = {
    "__REALTIME_TIMESTAMP",
    "__MONOTONIC_TIMESTAMP",
    "_BOOT_ID",
    "_MACHINE_ID",
    "_HOSTNAME",
    "PRIORITY",
    "_TRANSPORT",
    "SYSLOG_IDENTIFIER",
    "MESSAGE",
}
SYSTEMD_FIELDS = {
    "SYSLOG_FACILITY",
    "_UID",
    "_GID",
    "_PID",
    "_COMM",
    "_SYSTEMD_UNIT",
    "MESSAGE_ID",
    "UNIT",
    "JOB_TYPE",
}
UNIT_STDOUT_FIELDS = {"SYSLOG_FACILITY", "_UID", "_GID", "_PID", "_COMM", "_SYSTEMD_UNIT"}


def fixture_runs():
    return sorted(path.parent for path in FIXTURE_ROOT.rglob(EXPECTED_NAME))


def replay(run):
    return json.loads(json.dumps(timeline.build_timeline(run)))


def journal_rows(run):
    return [json.loads(line) for line in (run / JOURNAL_NAME).read_text().splitlines()]


def rfc3339(realtime_us):
    return (EPOCH + timedelta(microseconds=int(realtime_us))).isoformat(timespec="microseconds").replace(
        "+00:00", "Z"
    )


def max_hamming(values):
    return max(bin(a ^ b).count("1") for a, b in itertools.combinations(values, 2))


class FixtureReplayTest(unittest.TestCase):
    def test_baseline_fixture_is_committed(self):
        self.assertIn(BASELINE_RUN, fixture_runs())

    def test_bios_fixture_is_committed(self):
        self.assertIn(BIOS_RUN, fixture_runs())

    def test_every_fixture_matches_expected_timeline(self):
        for run in fixture_runs():
            with self.subTest(run=run.name):
                expected = json.loads((run / EXPECTED_NAME).read_text())
                self.assertEqual(replay(run), expected)

    def test_every_fixture_passes_its_profile(self):
        for run in fixture_runs():
            with self.subTest(run=run.name):
                name = (run / PROFILE_NAME).read_text().strip()
                self.assertIn(name, timeline.PROFILES)
                self.assertEqual(timeline.PROFILES[name](replay(run)), [])


class SyntheticBaselineStoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.timeline = replay(BASELINE_RUN)

    def test_anchor_is_fpdt_exit_boot_services_end(self):
        self.assertEqual(self.timeline["clock"]["kernel_zero_source"], "fpdt.exitbootservice_end")
        self.assertEqual(self.timeline["clock"]["kernel_zero_us"], KERNEL_ZERO_US)

    def test_greetd_start_is_the_started_entry(self):
        self.assertEqual(
            self.timeline["greetd_started"],
            {"t_us": KERNEL_ZERO_US + 3_541_208, "mono_us": 3_541_208},
        )

    def test_each_head_first_appears_after_greetd_start(self):
        self.assertEqual(
            self.timeline["greeter_first_appearance"],
            {
                HEAD_CRTC81: {"t_us": KERNEL_ZERO_US + 3_604_512, "mono_us": 3_604_512},
                HEAD_CRTC91: {"t_us": KERNEL_ZERO_US + 3_611_873, "mono_us": 3_611_873},
            },
        )

    def test_probe_types_the_password_into_the_greeter(self):
        # "wrongprobe" is ten keys; the Enter that follows makes eleven.
        self.assertEqual(
            [entry["line"] for entry in self.timeline["probe"]],
            [
                "device node=/dev/input/event9",
                "ready holder=niri pid=1431",
                "typed keys=11",
            ],
        )

    def test_initrd_sampler_keeps_sampling_after_a_retryable_error(self):
        errors = self.timeline["heads"][HEAD_CRTC91]["errors"]
        self.assertEqual(len(errors), 1)
        self.assertEqual(errors[0]["unit"], INITRD_UNIT)
        self.assertEqual(errors[0]["errno"], errno.EAGAIN)
        later = [
            sample["mono_us"]
            for sample in self.timeline["heads"][HEAD_CRTC81]["samples"]
            if sample["unit"] == INITRD_UNIT and sample["mono_us"] > errors[0]["mono_us"]
        ]
        self.assertEqual(later, [2_412_903])


class GnomonBaselineStoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.timeline = replay(GNOMON_RUN)

    def test_anchor_is_fpdt_exit_boot_services_end(self):
        self.assertEqual(self.timeline["clock"]["kernel_zero_source"], "fpdt.exitbootservice_end")
        self.assertEqual(self.timeline["clock"]["kernel_zero_us"], GNOMON_KERNEL_ZERO_US)

    def test_crc_heads_are_exactly_the_two_panel_heads(self):
        self.assertEqual(sorted(self.timeline["heads"]), [HEAD_CRTC205, HEAD_CRTC494])


class SyntheticBaselineShapeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rows = journal_rows(BASELINE_RUN)

    def test_every_value_is_a_string(self):
        for row in self.rows:
            for key, value in row.items():
                with self.subTest(key=key):
                    self.assertIsInstance(value, str)

    def test_every_line_carries_the_common_fields(self):
        for row in self.rows:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(COMMON_FIELDS, set(row))

    def test_kernel_lines_carry_no_user_or_group(self):
        kernel = [row for row in self.rows if row["_TRANSPORT"] == "kernel"]
        self.assertTrue(kernel)
        for row in kernel:
            with self.subTest(message=row["MESSAGE"]):
                self.assertIn("_SOURCE_MONOTONIC_TIMESTAMP", row)
                self.assertNotIn("_UID", row)
                self.assertNotIn("_GID", row)

    def test_systemd_lines_carry_their_job_fields(self):
        systemd = [row for row in self.rows if row.get("_PID") == "1"]
        self.assertTrue(systemd)
        for row in systemd:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(SYSTEMD_FIELDS, set(row))

    def test_unit_stdout_lines_carry_their_unit(self):
        unit = [row for row in self.rows if row["_TRANSPORT"] == "stdout"]
        self.assertTrue(unit)
        for row in unit:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(UNIT_STDOUT_FIELDS, set(row))


class BiosShaderGreeterStoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.timeline = replay(BIOS_RUN)
        cls.events = cls.timeline["events"]
        cls.halmasuit = [event for event in cls.events if event["kind"] == "halmasuit"]
        cls.frames = [event for event in cls.halmasuit if event["payload"]["event"] == "frame_rendered"]
        cls.display = [event for event in cls.events if event["kind"] in ("kernel-display", "driver-display")]

    def test_anchor_is_fpdt_exit_boot_services_end(self):
        self.assertEqual(self.timeline["clock"]["kernel_zero_source"], "fpdt.exitbootservice_end")
        self.assertEqual(self.timeline["clock"]["kernel_zero_us"], KERNEL_ZERO_US)

    def test_every_halmasuit_line_reaches_the_timeline(self):
        lines = [row for row in journal_rows(BIOS_RUN) if row.get("_SYSTEMD_UNIT") == HALMASUIT_UNIT]
        self.assertEqual(len(self.halmasuit), len(lines))
        self.assertEqual(len(self.frames), FRAME_COUNT)

    def test_halmasuit_story_events_are_in_order(self):
        story = [
            (event["mono_us"], event["payload"])
            for event in self.halmasuit
            if event["payload"]["event"] != "frame_rendered"
        ]
        self.assertEqual(story, EXPECTED_HALMASUIT_STORY)

    def test_frames_run_every_half_second_from_the_first_frame(self):
        self.assertEqual(
            [event["mono_us"] for event in self.frames],
            [FIRST_FRAME_MONO + FRAME_INTERVAL_US * k for k in range(FRAME_COUNT)],
        )
        self.assertEqual([event["payload"]["frame_id"] for event in self.frames], list(range(FRAME_COUNT)))

    def test_initial_render_is_frame_zero_between_wallpaper_and_scanout(self):
        scanout = next(
            event["mono_us"]
            for event in self.halmasuit
            if event["payload"] == {"event": "phase_entered", "phase": "scanout_active"}
        )
        self.assertEqual(self.frames[0]["payload"]["frame_id"], 0)
        self.assertLess(WALLPAPER_MONO, self.frames[0]["mono_us"])
        self.assertLess(self.frames[0]["mono_us"], scanout)

    def test_halmasuit_starts_in_initrd_and_survives_into_rootfs(self):
        self.assertEqual(
            [(start["unit"], start["mono_us"]) for start in self.timeline["sampler_starts"]],
            [(INITRD_UNIT, INITRD_SAMPLER_START_MONO), (ROOTFS_UNIT, ROOTFS_SAMPLER_START_MONO)],
        )
        started = self.halmasuit[0]
        self.assertEqual(started["payload"]["event"], "started")
        ready = next(
            event
            for event in self.halmasuit
            if event["payload"] == {"event": "phase_entered", "phase": "rootfs_ready"}
        )
        self.assertLess(started["mono_us"], ROOTFS_SAMPLER_START_MONO)
        self.assertLess(ROOTFS_SAMPLER_START_MONO, ready["mono_us"])

    def test_nvidia_driver_loads_before_drm_master(self):
        drivers = [event for event in self.display if event["kind"] == "driver-display"]
        self.assertEqual([event["detail"] for event in drivers], list(NVIDIA_LINES))
        for event in drivers:
            with self.subTest(detail=event["detail"]):
                self.assertLess(event["mono_us"], DRM_MASTER_MONO)

    def test_no_console_takeover_before_drm_master(self):
        details = [event["detail"] for event in self.display]
        self.assertFalse(any("Console: switching" in detail for detail in details))
        self.assertFalse(any("is primary device" in detail and "fbcon" in detail for detail in details))
        self.assertFalse(any("frame buffer device" in detail for detail in details))

    def test_wallpaper_is_the_one_first_frame_before_every_frame(self):
        wallpaper = [
            event["mono_us"]
            for event in self.halmasuit
            if event["payload"] == {"event": "client_first_frame", "role": "wallpaper"}
        ]
        self.assertEqual(wallpaper, [WALLPAPER_MONO])
        self.assertLess(WALLPAPER_MONO, FIRST_FRAME_MONO)

    def test_every_frame_is_wallpaper_covered_and_clean(self):
        self.assertEqual(len(self.frames), FRAME_COUNT)
        for event in self.frames:
            with self.subTest(frame_id=event["payload"]["frame_id"]):
                payload = event["payload"]
                self.assertEqual(payload["clear_pixel_count"], 0)
                self.assertEqual(payload["black_pixel_count"], 0)
                self.assertFalse(payload["degenerate"])
                self.assertEqual(payload["pixel_count"], FRAME_PIXELS)

    def test_frames_animate(self):
        phashes = [event["payload"]["phash"] for event in self.frames]
        self.assertGreaterEqual(len(set(phashes)), 5)
        self.assertGreaterEqual(max_hamming(set(phashes)), 10)

    def test_greeter_takes_foreground_before_the_overlay_first_frame(self):
        overlay_payload = {"event": "client_first_frame", "role": "overlay"}
        overlay = [event for event in self.halmasuit if event["payload"] == overlay_payload]
        self.assertEqual([event["mono_us"] for event in overlay], [OVERLAY_MONO])
        greeter_payload = {"event": "foreground_changed", "to": "greeter"}
        greeter = [event for event in self.halmasuit if event["payload"] == greeter_payload]
        self.assertEqual(
            greeter,
            [
                {
                    "t_us": KERNEL_ZERO_US + GREETER_MONO,
                    "mono_us": GREETER_MONO,
                    "kind": "halmasuit",
                    "source": HALMASUIT_UNIT,
                    "detail": "foreground_changed",
                    "payload": greeter_payload,
                }
            ],
        )
        self.assertLess(GREETER_MONO, OVERLAY_MONO)

    def test_panel_heads_are_exactly_the_two_panel_heads(self):
        self.assertEqual(sorted(self.timeline["heads"]), sorted(PANEL_HEADS))

    def test_each_panel_head_samples_every_tick_through_the_hold(self):
        for key in PANEL_HEADS:
            with self.subTest(head=key):
                samples = self.timeline["heads"][key]["samples"]
                expected = [tick + PANEL_SAMPLE_DELAY_US[key] for tick in SAMPLE_TICKS]
                self.assertEqual([sample["mono_us"] for sample in samples], expected)
                self.assertGreaterEqual(SAMPLE_TICKS[-1], HOLD_END_MONO)

    def test_panel_heads_sample_only_after_drm_master(self):
        for key in PANEL_HEADS:
            with self.subTest(head=key):
                samples = self.timeline["heads"][key]["samples"]
                self.assertTrue(samples)
                self.assertGreater(min(sample["mono_us"] for sample in samples), DRM_MASTER_MONO)

    def test_panel_heads_never_share_a_sample_time(self):
        first, second = (
            {sample["mono_us"] for sample in self.timeline["heads"][key]["samples"]} for key in PANEL_HEADS
        )
        self.assertTrue(first.isdisjoint(second))

    def test_panel_heads_hold_nonzero_changing_checksums_after_drm_master(self):
        for key in PANEL_HEADS:
            with self.subTest(head=key):
                after = [
                    sample["comp"]
                    for sample in self.timeline["heads"][key]["samples"]
                    if sample["mono_us"] > DRM_MASTER_MONO
                ]
                self.assertTrue(after)
                self.assertNotIn("00000000", after)
                self.assertGreaterEqual(len(set(after)), 2)
                zero_after_frame = [
                    sample
                    for sample in self.timeline["heads"][key]["samples"]
                    if sample["mono_us"] > FIRST_FRAME_MONO and sample["comp"] == "00000000"
                ]
                self.assertEqual(zero_after_frame, [])

    def test_every_frame_has_a_sample_on_each_head_within_a_quarter_second(self):
        self.assertEqual(len(self.frames), FRAME_COUNT)
        for event in self.frames:
            for key in PANEL_HEADS:
                with self.subTest(frame_id=event["payload"]["frame_id"], head=key):
                    samples = self.timeline["heads"][key]["samples"]
                    nearest = min(abs(sample["mono_us"] - event["mono_us"]) for sample in samples)
                    self.assertLessEqual(nearest, FRAME_SAMPLE_TOLERANCE_US)


class BiosShaderGreeterShapeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rows = journal_rows(BIOS_RUN)

    def test_every_value_is_a_string(self):
        for row in self.rows:
            for key, value in row.items():
                with self.subTest(key=key):
                    self.assertIsInstance(value, str)

    def test_every_line_carries_the_common_fields(self):
        for row in self.rows:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(COMMON_FIELDS, set(row))

    def test_kernel_lines_carry_source_time_and_no_user_or_group(self):
        kernel = [row for row in self.rows if row["_TRANSPORT"] == "kernel"]
        self.assertTrue(kernel)
        for row in kernel:
            with self.subTest(message=row["MESSAGE"]):
                self.assertIn("_SOURCE_MONOTONIC_TIMESTAMP", row)
                self.assertNotIn("_UID", row)
                self.assertNotIn("_GID", row)

    def test_systemd_lines_carry_their_job_fields(self):
        systemd = [row for row in self.rows if row.get("_PID") == "1"]
        self.assertTrue(systemd)
        for row in systemd:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(SYSTEMD_FIELDS, set(row))

    def test_unit_stdout_lines_carry_their_unit(self):
        unit = [row for row in self.rows if row["_TRANSPORT"] == "stdout"]
        self.assertTrue(unit)
        for row in unit:
            with self.subTest(message=row["MESSAGE"]):
                self.assertLessEqual(UNIT_STDOUT_FIELDS, set(row))

    def test_halmasuit_lines_are_compositor_stdout_with_tracing_json(self):
        halmasuit = [row for row in self.rows if row.get("_SYSTEMD_UNIT") == HALMASUIT_UNIT]
        self.assertTrue(halmasuit)
        for row in halmasuit:
            with self.subTest(message=row["MESSAGE"]):
                self.assertEqual(row["_TRANSPORT"], "stdout")
                self.assertEqual(row["SYSLOG_IDENTIFIER"], "halmasuit")
                self.assertEqual(row["_COMM"], "halmasuit")
                self.assertEqual(row["_UID"], HALMASUIT_STREAM_ID)
                self.assertEqual(row["_GID"], HALMASUIT_STREAM_ID)
                outer = json.loads(row["MESSAGE"])
                self.assertEqual(set(outer), {"timestamp", "level", "fields", "target"})
                self.assertEqual(outer["level"], "INFO")
                self.assertEqual(outer["target"], "halmasuit::event")
                self.assertEqual(set(outer["fields"]), {"json"})
                self.assertIn("event", json.loads(outer["fields"]["json"]))
                self.assertEqual(outer["timestamp"], rfc3339(row["__REALTIME_TIMESTAMP"]))

    def test_halmasuit_lines_come_from_one_process(self):
        pids = {row["_PID"] for row in self.rows if row.get("_SYSTEMD_UNIT") == HALMASUIT_UNIT}
        self.assertEqual(pids, {HALMASUIT_PID})

    def test_sample_lines_match_the_lab_crc_shape(self):
        samples = [row for row in self.rows if row.get("_SYSTEMD_UNIT") in (INITRD_UNIT, ROOTFS_UNIT)]
        messages = [row["MESSAGE"] for row in samples if row["MESSAGE"].startswith("sample ")]
        self.assertTrue(messages)
        for message in messages:
            with self.subTest(message=message):
                self.assertRegex(message, SAMPLE_LINE)


if __name__ == "__main__":
    unittest.main()
