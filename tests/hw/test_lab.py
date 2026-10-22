#!/usr/bin/env python3
"""Selftest for tests/hw/lab.py.

The runner drives a simulated gnomon through the Layer seam. The simulator
accepts only the command shapes written here and raises UnscriptedCommand on
anything else, so no ssh, nix, sbctl, or network call can reach a real host.
Polling deadlines elapse on FakeClock, so nothing sleeps.

The shapes follow the real tools: `nix build --eval-store auto --store ssh-ng://gnomon`
realises config.system.build.uki in gnomon's store and prints a directory
holding one .efi image that only gnomon can list; `sbctl sign -o OUT IN` signs with
sbctl's own db key; `bootctl list --json=short` marks the entry the loader
boots next with isDefault, a one-shot entry included; `bootctl set-oneshot ""`
clears the one-shot. `nix build` out paths are the fake's `/nix/store/aaaa-EXP-KIND`,
and `sudo -n ln -sfnT TARGET LINK` points a GC root at a build out path, replacing
any link already there; it fails while the GC root directory is missing.
"""

import contextlib
import io
import itertools
import json
import re
import tempfile
import unittest
from collections.abc import Mapping, Sequence
from pathlib import Path

import lab

NIX_CONFIG = "/nonexistent/nix-config"
REPO = "/nonexistent/halmasuit"
REV = "c" * 40
MAC = "02:00:00:0a:0b:0c"
OLD = "nixos-generation-6.efi"
PRUNED = "nixos-generation-5.efi"
DEFAULT = "nixos-generation-7.efi"
LAB = "halmasuit-lab.efi"
ESP = "/boot/EFI/Linux/halmasuit-lab.efi"
STAGED_DIR = "/var/lib/halmasuit-lab"
STAGED = "/var/lib/halmasuit-lab/halmasuit-lab.efi"
UKI_IMAGE_NAME = "lab-uki.efi"
BOOT_ID = "/proc/sys/kernel/random/boot_id"
SELECTED = (
    "sudo",
    "-n",
    "sh",
    "-c",
    "tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
    " | iconv -f UTF-16LE -t UTF-8",
)
MIB = 1024 * 1024
GIB = 1024 * MIB
UKI_BYTES = 60 * MIB
INSTALLABLE = re.compile(
    r"(?P<nix>.+)#labSystems\.gnomon\.(?P<exp>[a-z-]+)\.config\.system\.build\.(?P<kind>toplevel|uki)"
)
BUILD_PREFIX = (
    "nix",
    "build",
    "--eval-store",
    "auto",
    "--store",
    "ssh-ng://gnomon",
    "--no-link",
    "--print-out-paths",
)
UKI_DIR = re.compile(r"/nix/store/aaaa-(?P<exp>[a-z-]+)-uki")
UKI_IMAGE = re.compile(r"/nix/store/aaaa-[a-z-]+-uki/" + re.escape(UKI_IMAGE_NAME))
GC_ROOT_DIR = "/nix/var/nix/gcroots/halmasuit-lab"
GC_LINKS = {
    "toplevel": f"{GC_ROOT_DIR}/toplevel",
    "uki": f"{GC_ROOT_DIR}/uki",
}
TOPLEVEL_OUT = re.compile(r"/nix/store/aaaa-[a-z-]+-toplevel")
GC_TARGETS = {"toplevel": TOPLEVEL_OUT, "uki": UKI_DIR}


class UnscriptedCommand(AssertionError):
    """A command reached the simulator that no scripted shape accepts."""


def _ok(stdout: str = "") -> lab.Result:
    return lab.Result(0, stdout, "")


def bootctl_json(ids: Sequence[str], defaults: Sequence[str], reported: Sequence[str] = ()) -> str:
    """`bootctl list --json=short` output: one object per entry, isDefault marks the default.

    As on a real host, entries the boot loader reported at the current boot
    (LoaderEntries) but that are gone from the ESP appear as `loader` records,
    and the loader's built-in reboot-to-firmware entry appears as `auto`.
    """
    records = [
        {
            "type": "type2",
            "source": "esp",
            "id": entry,
            "path": f"/boot/EFI/Linux/{entry}",
            "title": "NixOS Generation",
            "isReported": entry in reported,
            "isDefault": entry in defaults,
            "isSelected": False,
        }
        for entry in ids
    ]
    records += [
        {
            "type": "loader",
            "source": "esp",
            "id": entry,
            "isReported": True,
            "isDefault": False,
            "isSelected": False,
        }
        for entry in sorted(set(reported) - set(ids))
    ]
    records.append(
        {
            "type": "auto",
            "source": "esp",
            "id": "auto-reboot-to-firmware-setup",
            "title": "Reboot Into Firmware Interface",
            "isReported": True,
            "isDefault": False,
            "isSelected": False,
        }
    )
    return json.dumps(records, indent=2)


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds

    def stamp(self) -> str:
        return "20260101T000000Z"


class FakeGnomon:
    """Simulated gnomon behind the Layer seam.

    lab_mode is what the lab image does when it boots: "boots" comes up as the
    lab, "returns" fails and falls back to the default, "poweroff" powers off.
    The machine is up with boot-1 running the default entry at the start.

    bootctl names the one-shot as the default whenever a one-shot is set, as
    the real tool does. install_moves_default makes the default change as soon
    as the lab entry lands in the ESP, before any one-shot is set.
    """

    def __init__(
        self,
        *,
        lab_mode: str = "boots",
        sessions: Sequence[tuple[str, str, str]] = (),
        esp_free: int = GIB,
        login_on_lab_boot: Sequence[tuple[str, str, str]] | None = None,
        install_moves_default: bool = False,
        return_adds_generation: bool = False,
        display_manager: str = "active",
        uki_files: Sequence[str] = (UKI_IMAGE_NAME,),
        gc_roots: Mapping[str, str] | None = None,
        fail_gc_root: str | None = None,
    ) -> None:
        self.lab_mode = lab_mode
        self.sessions = list(sessions)
        self.esp_free = esp_free
        self.login_on_lab_boot = None if login_on_lab_boot is None else list(login_on_lab_boot)
        self.install_moves_default = install_moves_default
        self.return_adds_generation = return_adds_generation
        self.display_manager = display_manager
        self.uki_files = list(uki_files)
        self.calls: list[tuple[str, tuple[str, ...]]] = []
        self.events: list[str] = []
        self.woken: list[str] = []
        self.up = True
        self.pending = False
        self.powered_off = False
        self.boots = 1
        self.current_boot = "boot-1"
        self.selected = DEFAULT
        self.default = DEFAULT
        self.esp = {DEFAULT, OLD}
        # LoaderEntries of the running boot: what the loader found on the ESP
        # when it started. PRUNED was reported at boot-1 and is gone since.
        self.reported = {DEFAULT, OLD, PRUNED}
        self.oneshot: str | None = None
        self.staged = False
        self.profiles = ["system-6-link", "system-7-link"]
        # gc_roots models links an earlier run left behind, so the directory exists
        # with them. fail_gc_root names the GC root whose `ln` is refused.
        self.gc_dir = gc_roots is not None
        self.gc_roots: dict[str, str] = dict(gc_roots or {})
        self.fail_gc_root = fail_gc_root

    def _boot(self) -> None:
        self.boots += 1
        self.current_boot = f"boot-{self.boots}"
        self.reported = set(self.esp)
        choice = self.oneshot or self.default
        self.oneshot = None
        if choice != LAB:
            self.selected = choice
            self.up = True
            if self.return_adds_generation:
                newest = max(int(name.split("-")[1]) for name in self.profiles)
                self.profiles.append(f"system-{newest + 1}-link")
            return
        self.selected = LAB
        if self.login_on_lab_boot is not None:
            self.sessions = list(self.login_on_lab_boot)
        self.up = self.lab_mode == "boots"
        self.pending = self.lab_mode == "returns"
        self.powered_off = self.lab_mode == "poweroff"

    def _is_up(self) -> bool:
        if not self.up and self.pending:
            self.pending = False
            self._boot()
        return self.up

    def _listing(self) -> str:
        if self.oneshot is not None and self.oneshot in self.esp:
            default = self.oneshot
        elif self.install_moves_default and LAB in self.esp:
            default = LAB
        else:
            default = self.default
        return bootctl_json(sorted(self.esp), [default], sorted(self.reported))

    def local(self, argv: Sequence[str]) -> lab.Result:
        command = tuple(argv)
        self.calls.append(("local", command))
        if command == ("git", "-C", REPO, "rev-parse", "HEAD"):
            return _ok(f"{REV}\n")
        if len(command) == 12 and command[:8] == BUILD_PREFIX:
            match = INSTALLABLE.fullmatch(command[8])
            if (
                match is None
                or match["nix"] != NIX_CONFIG
                or command[9:] != ("--override-input", "halmasuit", f"git+file://{REPO}?rev={REV}")
            ):
                raise UnscriptedCommand(command)
            self.events.append(f"build:{match['exp']}:{match['kind']}")
            return _ok(f"/nix/store/aaaa-{match['exp']}-{match['kind']}\n")
        raise UnscriptedCommand(command)

    def remote(self, argv: Sequence[str]) -> lab.Result:
        command = tuple(argv)
        self.calls.append(("remote", command))
        if not self._is_up():
            return lab.Result(255, "", "ssh: connect to host gnomon: no route to host")
        return self._gnomon(command)

    def _gnomon(self, argv: tuple[str, ...]) -> lab.Result:
        if argv == ("cat", BOOT_ID):
            return _ok(f"{self.current_boot}\n")
        if argv == ("loginctl", "list-sessions", "--no-legend"):
            return _ok("".join(f"{sid} 1000 josh - -\n" for sid, _, _ in self.sessions))
        if (
            len(argv) == 7
            and argv[:2] == ("loginctl", "show-session")
            and argv[3:] == ("-p", "Class", "-p", "Type")
        ):
            for sid, cls, typ in self.sessions:
                if sid == argv[2]:
                    return _ok(f"Class={cls}\nType={typ}\n")
            return lab.Result(1, "", "no such session")
        if argv == ("sudo", "-n", "rm", "-f", ESP):
            self.esp.discard(LAB)
            self.events.append("rm-entry")
            return _ok()
        if argv == ("sudo", "-n", "install", "-d", "-m", "0755", GC_ROOT_DIR):
            self.gc_dir = True
            return _ok()
        if len(argv) == 6 and argv[:4] == ("sudo", "-n", "ln", "-sfnT") and argv[5] in GC_LINKS.values():
            return self._link_root(argv)
        if argv == ("sudo", "-n", "install", "-d", "-m", "0700", STAGED_DIR):
            return _ok()
        if (
            len(argv) == 7
            and argv[:4] == ("sudo", "-n", "sbctl", "sign")
            and argv[4:6] == ("-o", STAGED)
            and UKI_IMAGE.fullmatch(argv[6])
        ):
            self.staged = True
            self.events.append("stage")
            return _ok()
        if argv == ("df", "-B1", "--output=avail", "/boot"):
            return _ok(f"     Avail\n{self.esp_free}\n")
        if len(argv) == 4 and argv[:3] == ("stat", "-c", "%s") and UKI_IMAGE.fullmatch(argv[3]):
            return _ok(f"{UKI_BYTES}\n")
        if argv == ("sudo", "-n", "bootctl", "list", "--json=short"):
            return _ok(self._listing())
        if len(argv) == 3 and argv[:2] == ("ls", "-1") and UKI_DIR.fullmatch(argv[2]):
            return _ok("".join(f"{name}\n" for name in self.uki_files))
        if argv == ("ls", "-1", "/nix/var/nix/profiles"):
            return _ok("".join(f"{name}\n" for name in self.profiles))
        if argv == ("sudo", "-n", "cp", STAGED, ESP):
            if not self.staged:
                return lab.Result(1, "", "cp: cannot stat staged image")
            self.esp.add(LAB)
            self.events.append("install")
            return _ok()
        if argv == ("sudo", "-n", "bootctl", "set-oneshot", LAB):
            self.oneshot = LAB
            self.events.append(f"oneshot:{LAB}")
            return _ok()
        if argv == ("sudo", "-n", "bootctl", "set-oneshot", ""):
            self.oneshot = None
            self.events.append("oneshot-clear")
            return _ok()
        if argv == ("sudo", "-n", "systemctl", "reboot"):
            self.events.append("reboot")
            self.up = False
            self.pending = True
            return _ok()
        if argv == ("cat", "/sys/class/net/enp7s0/address"):
            return _ok(f"{MAC}\n")
        if argv == ("systemctl", "is-active", "display-manager.service"):
            if self.display_manager == "active":
                return _ok("active\n")
            return lab.Result(3, "inactive\n", "")
        if argv == SELECTED:
            return _ok(f"{self.selected}\x00\n")
        raise UnscriptedCommand(argv)

    def _link_root(self, argv: tuple[str, ...]) -> lab.Result:
        target, link = argv[4], argv[5]
        kind = next(name for name, path in GC_LINKS.items() if path == link)
        if not GC_TARGETS[kind].fullmatch(target):
            raise UnscriptedCommand(argv)
        if not self.gc_dir:
            return lab.Result(1, "", f"ln: failed to create symbolic link '{link}': No such file or directory")
        if kind == self.fail_gc_root:
            return lab.Result(1, "", f"ln: failed to create symbolic link '{link}': Read-only file system")
        self.gc_roots[kind] = target
        self.events.append(f"root:{kind}")
        return _ok()

    def wake(self, mac: str) -> None:
        self.woken.append(mac)
        self.events.append("wake")
        if self.powered_off and mac == MAC:
            self.powered_off = False
            self.pending = True


BUILDS_AND_ROOTS = [
    "build:{exp}:toplevel",
    "build:{exp}:uki",
    "root:toplevel",
    "root:uki",
]


def fault_sequence(exp: str) -> list[str]:
    return [
        *(event.format(exp=exp) for event in BUILDS_AND_ROOTS),
        "stage",
        "rm-entry",
        "install",
        f"oneshot:{LAB}",
        "reboot",
        "rm-entry",
    ]


class LabRunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.runs = itertools.count()
        self.state_root = self.tmp

    def run_queue(
        self, names: Sequence[str], gnomon: FakeGnomon
    ) -> tuple[int, str, str]:
        self.state_root = self.tmp / f"runs-{next(self.runs)}"
        argv = [
            "--nix-config",
            NIX_CONFIG,
            "--repo-root",
            REPO,
            "--state-root",
            str(self.state_root),
            *names,
        ]
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = lab.main(argv, layer=gnomon, clock=FakeClock())
        return code, out.getvalue(), err.getvalue()

    def single_run_dir(self, name: str) -> Path:
        found = sorted(self.state_root.glob(f"*-{name}"))
        self.assertEqual(len(found), 1, found)
        return found[0]

    def test_images_are_built_in_gnomon_store_from_the_pinned_head(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        for kind in ("toplevel", "uki"):
            with self.subTest(kind=kind):
                argv = (
                    "nix",
                    "build",
                    "--eval-store",
                    "auto",
                    "--store",
                    "ssh-ng://gnomon",
                    "--no-link",
                    "--print-out-paths",
                    f"{NIX_CONFIG}#labSystems.gnomon.baseline.config.system.build.{kind}",
                    "--override-input",
                    "halmasuit",
                    f"git+file://{REPO}?rev={REV}",
                )
                self.assertIn(("local", argv), gnomon.calls)

    def test_uki_directory_is_listed_on_gnomon(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertIn(("remote", ("ls", "-1", "/nix/store/aaaa-baseline-uki")), gnomon.calls)

    def test_no_nix_copy_is_issued(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual([argv for _, argv in gnomon.calls if argv[:2] == ("nix", "copy")], [])

    def test_baseline_boots_lab_then_returns_to_default(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual(
            gnomon.events,
            [
                "build:baseline:toplevel",
                "build:baseline:uki",
                "root:toplevel",
                "root:uki",
                "stage",
                "rm-entry",
                "install",
                f"oneshot:{LAB}",
                "reboot",
                "reboot",
                "rm-entry",
            ],
        )
        self.assertEqual(gnomon.woken, [])
        self.assertEqual(gnomon.esp, {DEFAULT, OLD})
        self.assertIsNone(gnomon.oneshot)
        self.assertEqual(gnomon.selected, DEFAULT)
        self.assertIn("PASS", (self.single_run_dir("baseline") / "run.log").read_text())

    def test_gc_roots_point_at_both_out_paths_and_survive_success(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual(
            gnomon.gc_roots,
            {
                "toplevel": "/nix/store/aaaa-baseline-toplevel",
                "uki": "/nix/store/aaaa-baseline-uki",
            },
        )
        self.assertIn(
            ("remote", ("sudo", "-n", "install", "-d", "-m", "0755", "/nix/var/nix/gcroots/halmasuit-lab")),
            gnomon.calls,
        )
        self.assertIn(
            (
                "remote",
                (
                    "sudo",
                    "-n",
                    "ln",
                    "-sfnT",
                    "/nix/store/aaaa-baseline-toplevel",
                    "/nix/var/nix/gcroots/halmasuit-lab/toplevel",
                ),
            ),
            gnomon.calls,
        )
        self.assertIn(
            (
                "remote",
                (
                    "sudo",
                    "-n",
                    "ln",
                    "-sfnT",
                    "/nix/store/aaaa-baseline-uki",
                    "/nix/var/nix/gcroots/halmasuit-lab/uki",
                ),
            ),
            gnomon.calls,
        )

    def test_gc_root_failure_stops_before_staging_and_install(self) -> None:
        uki_dir = "/nix/store/aaaa-fault-panic-uki"
        for kind, expected in (
            ("toplevel", ["build:fault-panic:toplevel", "build:fault-panic:uki"]),
            ("uki", ["build:fault-panic:toplevel", "build:fault-panic:uki", "root:toplevel"]),
        ):
            with self.subTest(failing_root=kind):
                gnomon = FakeGnomon(lab_mode="returns", fail_gc_root=kind)
                code, _, err = self.run_queue(["fault-panic"], gnomon)
                self.assertEqual(code, 1)
                self.assertIn(f"point {kind} GC root failed", err)
                self.assertEqual(gnomon.events, expected)
                self.assertNotIn(("remote", ("ls", "-1", uki_dir)), gnomon.calls)
                self.assertNotIn(LAB, gnomon.esp)
                self.assertIsNone(gnomon.oneshot)

    def test_gc_roots_survive_abort_and_only_install_and_ln_touch_them(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", display_manager="inactive")
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("display-manager.service", err)
        self.assertEqual(gnomon.events[-2:], ["rm-entry", "oneshot-clear"])
        self.assertEqual(
            gnomon.gc_roots,
            {
                "toplevel": "/nix/store/aaaa-fault-panic-toplevel",
                "uki": "/nix/store/aaaa-fault-panic-uki",
            },
        )
        touching = [argv for _, argv in gnomon.calls if any(GC_ROOT_DIR in part for part in argv)]
        self.assertEqual(
            touching,
            [
                ("sudo", "-n", "install", "-d", "-m", "0755", GC_ROOT_DIR),
                ("sudo", "-n", "ln", "-sfnT", "/nix/store/aaaa-fault-panic-toplevel", GC_LINKS["toplevel"]),
                ("sudo", "-n", "ln", "-sfnT", "/nix/store/aaaa-fault-panic-uki", GC_LINKS["uki"]),
            ],
        )

    def test_second_run_replaces_existing_gc_roots(self) -> None:
        old = {"toplevel": "/nix/store/aaaa-old-toplevel", "uki": "/nix/store/aaaa-old-uki"}
        gnomon = FakeGnomon(gc_roots=old)
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual(
            gnomon.gc_roots,
            {
                "toplevel": "/nix/store/aaaa-baseline-toplevel",
                "uki": "/nix/store/aaaa-baseline-uki",
            },
        )

    def test_failed_replacement_leaves_the_previous_link_in_place(self) -> None:
        old = {"toplevel": "/nix/store/aaaa-old-toplevel", "uki": "/nix/store/aaaa-old-uki"}
        gnomon = FakeGnomon(gc_roots=old, fail_gc_root="uki")
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 1)
        self.assertEqual(
            gnomon.gc_roots,
            {"toplevel": "/nix/store/aaaa-baseline-toplevel", "uki": "/nix/store/aaaa-old-uki"},
        )

    def test_fault_experiments_return_unaided_after_one_reboot(self) -> None:
        for name in ("fault-initrd", "fault-hang", "fault-panic"):
            with self.subTest(name=name):
                gnomon = FakeGnomon(lab_mode="returns")
                code, _, err = self.run_queue([name], gnomon)
                self.assertEqual(code, 0, err)
                self.assertEqual(gnomon.events, fault_sequence(name))
                self.assertEqual(gnomon.woken, [])
                self.assertEqual(gnomon.selected, DEFAULT)

    def test_fault_poweroff_wakes_gnomon_only_after_power_off(self) -> None:
        gnomon = FakeGnomon(lab_mode="poweroff")
        code, _, err = self.run_queue(["fault-poweroff"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual(
            gnomon.events,
            [
                *(event.format(exp="fault-poweroff") for event in BUILDS_AND_ROOTS),
                "stage",
                "rm-entry",
                "install",
                f"oneshot:{LAB}",
                "reboot",
                "wake",
                "rm-entry",
            ],
        )
        self.assertEqual(gnomon.woken, [MAC])
        self.assertEqual(gnomon.selected, DEFAULT)

    def test_queue_stops_at_first_failure(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns")
        code, _, err = self.run_queue(["baseline", "fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("queue stopped", err)
        self.assertEqual(
            gnomon.events,
            [
                *(event.format(exp="baseline") for event in BUILDS_AND_ROOTS),
                "stage",
                "rm-entry",
                "install",
                f"oneshot:{LAB}",
                "reboot",
                "rm-entry",
                "oneshot-clear",
            ],
        )
        self.assertNotIn(LAB, gnomon.esp)
        self.assertIsNone(gnomon.oneshot)

    def test_stale_lab_entry_is_removed_before_install(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns")
        gnomon.esp.add(LAB)
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertLess(gnomon.events.index("rm-entry"), gnomon.events.index("install"))
        self.assertEqual(gnomon.events.count("rm-entry"), 2)
        self.assertNotIn(LAB, gnomon.esp)

    def test_uki_directory_must_hold_exactly_one_image(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", uki_files=(UKI_IMAGE_NAME, "extra.efi"))
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("exactly one", err)
        self.assertNotIn("stage", gnomon.events)

    def test_esp_room_must_cover_image_plus_200_mib(self) -> None:
        image_and_margin = UKI_BYTES + 200 * MIB
        with self.subTest("exactly enough room runs"):
            gnomon = FakeGnomon(lab_mode="returns", esp_free=image_and_margin)
            code, _, err = self.run_queue(["fault-panic"], gnomon)
            self.assertEqual(code, 0, err)
        with self.subTest("one byte short refuses before install"):
            gnomon = FakeGnomon(lab_mode="returns", esp_free=image_and_margin - 1)
            code, _, err = self.run_queue(["fault-panic"], gnomon)
            self.assertEqual(code, 1)
            self.assertIn("ESP has", err)
            self.assertNotIn("install", gnomon.events)
            self.assertNotIn("reboot", gnomon.events)
            self.assertNotIn(LAB, gnomon.esp)

    def test_default_change_on_install_aborts_and_removes_entry(self) -> None:
        gnomon = FakeGnomon(install_moves_default=True)
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("pre-run default", err)
        self.assertNotIn("reboot", gnomon.events)
        self.assertNotIn(f"oneshot:{LAB}", gnomon.events)
        self.assertNotIn(LAB, gnomon.esp)
        self.assertIsNone(gnomon.oneshot)
        self.assertEqual(gnomon.events[-2:], ["rm-entry", "oneshot-clear"])

    def test_post_run_record_mismatch_fails(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", return_adds_generation=True)
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("generations", err)
        self.assertNotIn(LAB, gnomon.esp)

    def test_display_manager_must_be_active_after_return(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", display_manager="inactive")
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("display-manager.service", err)
        self.assertNotIn(LAB, gnomon.esp)

    def test_greeter_only_session_proceeds(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", sessions=[("c1", "greeter", "wayland")])
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 0, err)

    def test_tty_session_is_ignored(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", sessions=[("c2", "user", "tty")])
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 0, err)

    def test_logged_in_graphical_session_refuses_without_install_or_reboot(self) -> None:
        for session_type in ("wayland", "x11", "mir"):
            with self.subTest(session_type=session_type):
                gnomon = FakeGnomon(sessions=[("c3", "user", session_type)])
                code, _, err = self.run_queue(["fault-panic"], gnomon)
                self.assertEqual(code, 1)
                self.assertIn("refusing before install", err)
                self.assertEqual(
                    [e for e in gnomon.events if e in ("install", "reboot") or e.startswith("oneshot")],
                    [],
                )
                self.assertNotIn(LAB, gnomon.esp)

    def test_graphical_login_during_lab_boot_refuses_return_reboot(self) -> None:
        gnomon = FakeGnomon(login_on_lab_boot=[("c4", "user", "wayland")])
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("refusing before return reboot", err)
        self.assertEqual(gnomon.events.count("reboot"), 1)
        self.assertEqual(gnomon.events[-2:], ["rm-entry", "oneshot-clear"])
        self.assertNotIn(LAB, gnomon.esp)

    def test_missing_default_entry_refuses_before_install(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns")
        gnomon.default = "nixos-generation-9.efi"
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("default entries", err)
        self.assertNotIn("install", gnomon.events)

    def test_unknown_experiment_is_refused_before_any_action(self) -> None:
        gnomon = FakeGnomon()
        with self.assertRaises(SystemExit) as caught:
            self.run_queue(["baseline", "bogus"], gnomon)
        self.assertEqual(caught.exception.code, 2)
        self.assertEqual(gnomon.calls, [])


class MagicPacketTest(unittest.TestCase):
    def test_packet_is_six_ff_bytes_then_the_mac_sixteen_times(self) -> None:
        packet = lab.magic_packet(MAC)
        mac_bytes = bytes([0x02, 0x00, 0x00, 0x0A, 0x0B, 0x0C])
        self.assertEqual(packet, b"\xff" * 6 + mac_bytes * 16)
        self.assertEqual(len(packet), 102)


class BootctlParseTest(unittest.TestCase):
    def test_names_every_entry_and_the_single_default(self) -> None:
        entries, default = lab.parse_bootctl_list(bootctl_json([DEFAULT, OLD], [DEFAULT]))
        self.assertEqual(entries, [DEFAULT, OLD])
        self.assertEqual(default, DEFAULT)

    def test_loader_reported_and_auto_records_are_not_entries(self) -> None:
        listing = bootctl_json([DEFAULT, OLD], [DEFAULT], reported=[DEFAULT, OLD, PRUNED, LAB])
        entries, default = lab.parse_bootctl_list(listing)
        self.assertEqual(entries, [DEFAULT, OLD])
        self.assertEqual(default, DEFAULT)

    def test_no_default_is_refused(self) -> None:
        with self.assertRaises(lab.LabError):
            lab.parse_bootctl_list(bootctl_json([DEFAULT, OLD], []))

    def test_two_defaults_are_refused(self) -> None:
        with self.assertRaises(lab.LabError):
            lab.parse_bootctl_list(bootctl_json([DEFAULT, OLD], [DEFAULT, OLD]))

    def test_title_text_is_not_the_default_marker(self) -> None:
        listing = json.dumps(
            [{"id": DEFAULT, "title": "NixOS Generation 7 (default) (not reported/new)", "isDefault": False}]
        )
        with self.assertRaises(lab.LabError):
            lab.parse_bootctl_list(listing)

    def test_text_without_json_is_refused(self) -> None:
        with self.assertRaises(lab.LabError):
            lab.parse_bootctl_list("NixOS Generation 7 (default)\n")

    def test_generations_are_the_system_profile_numbers(self) -> None:
        self.assertEqual(
            lab.parse_generations("system-7-link\nsystem-6-link\nsystem-link\ncurrent\n"),
            (6, 7),
        )


if __name__ == "__main__":
    unittest.main()
