#!/usr/bin/env python3
"""Replays every committed fixture run through timeline.py.

A fixture run is a directory under tests/hw/fixtures/ that holds
`expected-timeline.json`; its `profile` file names the profile that must pass.
Runs are read in place: build_timeline never writes into the fixture. The
synthetic-baseline and gnomon-baseline runs also assert their story facts by
hand, so a golden file regenerated from a broken timeline still fails.
"""

import errno
import json
import unittest
from pathlib import Path

import timeline

FIXTURE_ROOT = Path(__file__).resolve().parent / "fixtures"
EXPECTED_NAME = "expected-timeline.json"
PROFILE_NAME = "profile"
JOURNAL_NAME = "journal.json"
BASELINE_RUN = FIXTURE_ROOT / "synthetic-baseline"
GNOMON_RUN = FIXTURE_ROOT / "gnomon-baseline"
# exitbootservice_end_ns is 2_583_541_217, which floors to 2_583_541 us.
KERNEL_ZERO_US = 2_583_541
# Recorded gnomon boot: exitbootservice_end_ns floors to 19_664_764 us.
GNOMON_KERNEL_ZERO_US = 19_664_764
INITRD_UNIT = "lab-crc-initrd.service"
HEAD_CRTC81 = "card1/crtc81"
HEAD_CRTC91 = "card1/crtc91"
HEAD_CRTC205 = "card1/crtc205"
HEAD_CRTC494 = "card1/crtc494"
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


class FixtureReplayTest(unittest.TestCase):
    def test_baseline_fixture_is_committed(self):
        self.assertIn(BASELINE_RUN, fixture_runs())

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


if __name__ == "__main__":
    unittest.main()
