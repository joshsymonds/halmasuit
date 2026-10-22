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
Boot ids are dashed UUIDs as /proc/sys/kernel/random/boot_id prints them;
`journalctl --list-boots -o json` prints them without dashes; `journalctl -b ID -o json` takes
only the undashed form (a dashed id fails with "Failed to add match 'ID': Invalid argument") and
fails with "Data from the specified boot (ID) is not available" for a boot with no journal.
"""

import base64
import contextlib
import io
import itertools
import json
import re
import socket
import tempfile
import threading
import unittest
import uuid
from collections.abc import Callable, Mapping, Sequence
from datetime import datetime, timedelta, timezone
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
LOADER_GUID = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
# Remote path -> run-directory destination (frozen run layout E1-D6).
FIRMWARE_LAYOUT = {
    f"/sys/firmware/{name}": f"firmware/{name}"
    for name in (
        "acpi/tables/FPDT",
        "acpi/fpdt/boot/firmware_start_ns",
        "acpi/fpdt/boot/bootloader_load_ns",
        "acpi/fpdt/boot/bootloader_launch_ns",
        "acpi/fpdt/boot/exitbootservice_start_ns",
        "acpi/fpdt/boot/exitbootservice_end_ns",
        "acpi/bgrt/status",
        "acpi/bgrt/type",
        "acpi/bgrt/version",
        "acpi/bgrt/xoffset",
        "acpi/bgrt/yoffset",
        "acpi/bgrt/image",
        f"efi/efivars/LoaderTimeInitUSec-{LOADER_GUID}",
        f"efi/efivars/LoaderTimeExecUSec-{LOADER_GUID}",
        f"efi/efivars/LoaderFirmwareInfo-{LOADER_GUID}",
    )
}
FIRMWARE_LAYOUT["/sys/kernel/boot_params/data"] = "firmware/boot_params/data"


def firmware_fixtures() -> dict[str, bytes]:
    """Each firmware file's bytes in the shape the kernel serves it.

    sysfs files are decimal text with a trailing newline; efivarfs files start with a
    4-byte attributes word, and systemd-boot's variables hold NUL-terminated UTF-16LE text
    (LoaderTime* a decimal microsecond count, LoaderFirmwareInfo the firmware vendor);
    FPDT, BGRT image and boot_params are binary and include bytes that text mode would mangle.
    """
    binary = bytes(range(256)) + b"\r\n\x00\xff"
    attributes = b"\x06\x00\x00\x00"
    return {
        "/sys/firmware/acpi/tables/FPDT": b"FPDT" + binary,
        "/sys/firmware/acpi/fpdt/boot/firmware_start_ns": b"1004000\n",
        "/sys/firmware/acpi/fpdt/boot/bootloader_load_ns": b"2008000\n",
        "/sys/firmware/acpi/fpdt/boot/bootloader_launch_ns": b"3012000\n",
        "/sys/firmware/acpi/fpdt/boot/exitbootservice_start_ns": b"4016000\n",
        "/sys/firmware/acpi/fpdt/boot/exitbootservice_end_ns": b"5020000\n",
        "/sys/firmware/acpi/bgrt/status": b"1\n",
        "/sys/firmware/acpi/bgrt/type": b"0\n",
        "/sys/firmware/acpi/bgrt/version": b"1\n",
        "/sys/firmware/acpi/bgrt/xoffset": b"312\n",
        "/sys/firmware/acpi/bgrt/yoffset": b"184\n",
        "/sys/firmware/acpi/bgrt/image": b"BM" + binary,
        "/sys/kernel/boot_params/data": binary * 4,
        f"/sys/firmware/efi/efivars/LoaderTimeInitUSec-{LOADER_GUID}": attributes
        + "1234567\x00".encode("utf-16-le"),
        f"/sys/firmware/efi/efivars/LoaderTimeExecUSec-{LOADER_GUID}": attributes
        + "2345678\x00".encode("utf-16-le"),
        f"/sys/firmware/efi/efivars/LoaderFirmwareInfo-{LOADER_GUID}": attributes
        + "EDK II 1.00\x00".encode("utf-16-le"),
    }


class UnscriptedCommand(AssertionError):
    """A command reached the simulator that no scripted shape accepts."""


RECEIVED = datetime(2026, 10, 21, 12, 0, 0, 123456, tzinfo=timezone.utc)
RECEIVED_STAMP = "2026-10-21T12:00:00.123456+00:00"


def _ok(stdout: str = "") -> lab.Result:
    return lab.Result(0, stdout, "")


def boot_uuid(label: str) -> str:
    """A dashed, lowercase UUID shaped like /proc/sys/kernel/random/boot_id, stable per label."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"halmasuit-test-boot:{label}"))


def journal_of(boot: str) -> str:
    """A boot's `journalctl -o json` output: one compact object per line, carrying the boot's undashed id."""
    undashed = boot.replace("-", "")
    records = [
        {"__REALTIME_TIMESTAMP": "1000001", "_BOOT_ID": undashed, "MESSAGE": f"kernel line for {boot}"},
        {"__REALTIME_TIMESTAMP": "1000002", "_BOOT_ID": undashed, "MESSAGE": "unicode é survives"},
    ]
    return "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records)


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


class FakeNetconsole:
    """The listener the fake layer hands out. Datagrams reach it only while it is open."""

    def __init__(self, trace: list[str], on_datagram: Callable[[datetime, bytes], None]) -> None:
        self.trace = trace
        self.on_datagram = on_datagram
        self.open = True

    def deliver(self, datagram: bytes) -> None:
        self.on_datagram(RECEIVED, datagram)

    def stop(self) -> None:
        self.open = False
        self.trace.append("listen:stop")


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
        bind_fails: bool = False,
        deliveries: Mapping[str, Sequence[bytes]] | None = None,
        firmware: Mapping[str, bytes] | None = None,
        firmware_garbled: Sequence[str] = (),
        lab_journal: bool = True,
        list_boots_fails: bool = False,
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
        self.current_boot = boot_uuid("boot-1")
        # persisted maps each boot with a journal to its `journalctl -o json` text, oldest first.
        # Two boots from before the run are already on disk, as on a machine that has run for a while.
        self.persisted: dict[str, str] = {
            boot_uuid("older-1"): journal_of(boot_uuid("older-1")),
            boot_uuid("older-2"): journal_of(boot_uuid("older-2")),
            self.current_boot: journal_of(self.current_boot),
        }
        # lab_journal False models the initramfs-only lab boot, which never persists a journal.
        self.lab_journal = lab_journal
        self.list_boots_fails = list_boots_fails
        self.lab_boot: str | None = None
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
        # trace interleaves gnomon events, emitted lines, and listener start/stop in order.
        self.trace: list[str] = []
        self.bind_fails = bind_fails
        # deliveries maps an event to the datagrams the netconsole stream sends right after it.
        self.deliveries = dict(deliveries or {})
        self.netconsole_ports: list[int] = []
        self.listener: FakeNetconsole | None = None
        # firmware maps a path to the bytes gnomon serves; a path not in it is absent.
        # firmware_garbled names paths whose base64 output is not valid base64.
        self.firmware = dict(firmware or {})
        self.firmware_garbled = tuple(firmware_garbled)

    def say(self, message: str) -> None:
        self.trace.append(f"say:{message}")

    def netconsole(self, port: int, on_datagram: Callable[[datetime, bytes], None]) -> FakeNetconsole:
        if self.bind_fails:
            raise lab.LabError(f"cannot listen for netconsole on UDP port {port}: Address already in use")
        self.netconsole_ports.append(port)
        self.trace.append("listen:start")
        self.listener = FakeNetconsole(self.trace, on_datagram)
        return self.listener

    def _event(self, event: str) -> None:
        self.events.append(event)
        self.trace.append(event)
        if self.listener is not None and self.listener.open:
            for datagram in self.deliveries.get(event, ()):
                self.listener.deliver(datagram)

    def _boot(self) -> None:
        self.boots += 1
        self.current_boot = boot_uuid(f"boot-{self.boots}")
        self.reported = set(self.esp)
        choice = self.oneshot or self.default
        self.oneshot = None
        if choice == LAB:
            self.lab_boot = self.current_boot
        if choice != LAB or self.lab_journal:
            self.persisted[self.current_boot] = journal_of(self.current_boot)
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
            self._event(f"build:{match['exp']}:{match['kind']}")
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
            self._event("rm-entry")
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
            self._event("stage")
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
            self._event("install")
            return _ok()
        if argv == ("sudo", "-n", "bootctl", "set-oneshot", LAB):
            self.oneshot = LAB
            self._event(f"oneshot:{LAB}")
            return _ok()
        if argv == ("sudo", "-n", "bootctl", "set-oneshot", ""):
            self.oneshot = None
            self._event("oneshot-clear")
            return _ok()
        if argv == ("sudo", "-n", "systemctl", "reboot"):
            self._event("reboot")
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
        if len(argv) == 5 and argv[:4] == ("sudo", "-n", "base64", "-w0") and argv[4] in FIRMWARE_LAYOUT:
            return self._base64(argv[4])
        if argv == ("sudo", "-n", "journalctl", "--list-boots", "-o", "json", "--no-pager"):
            return self._list_boots()
        if len(argv) == 8 and argv[:4] == ("sudo", "-n", "journalctl", "-b") and argv[5:] == (
            "-o",
            "json",
            "--no-pager",
        ):
            return self._read_journal(argv[4])
        raise UnscriptedCommand(argv)

    def _list_boots(self) -> lab.Result:
        """`journalctl --list-boots -o json`: every boot with a journal, oldest first, ids undashed."""
        self.trace.append("journal:list")
        if self.list_boots_fails:
            return lab.Result(1, "", "Failed to list boots: Input/output error")
        boots = list(self.persisted)
        records = [
            {
                "index": index - (len(boots) - 1),
                "boot_id": boot.replace("-", ""),
                "first_entry": 1000 * (index + 1),
                "last_entry": 1000 * (index + 1) + 999,
            }
            for index, boot in enumerate(boots)
        ]
        return _ok(json.dumps(records, indent=4) + "\n")

    def _read_journal(self, boot: str) -> lab.Result:
        """`journalctl -b ID -o json`: the boot's journal, or the error journalctl gives for a boot with none."""
        self.trace.append(f"journal:read:{boot}")
        if not re.fullmatch(r"[0-9a-f]{32}", boot):
            return lab.Result(1, "", f"Failed to add match '{boot}': Invalid argument")
        for stored, text in self.persisted.items():
            if stored.replace("-", "") == boot:
                return _ok(text)
        return lab.Result(1, "", f"Data from the specified boot ({boot}) is not available")

    def _base64(self, path: str) -> lab.Result:
        """GNU `base64 -w0`: one line, no trailing newline; a missing file exits 1."""
        self.trace.append(f"base64:{path}")
        if path in self.firmware_garbled:
            return _ok("!!not base64!!")
        if path not in self.firmware:
            return lab.Result(1, "", f"base64: {path}: No such file or directory")
        return _ok(base64.b64encode(self.firmware[path]).decode("ascii"))

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
        self._event(f"root:{kind}")
        return _ok()

    def wake(self, mac: str) -> None:
        self.woken.append(mac)
        self._event("wake")
        if self.powered_off and mac == MAC:
            self.powered_off = False
            self.pending = True


BUILDS_AND_ROOTS = [
    "build:{exp}:toplevel",
    "build:{exp}:uki",
    "root:toplevel",
    "root:uki",
]


def base64_calls(gnomon: FakeGnomon) -> list[tuple[str, ...]]:
    return [argv for _, argv in gnomon.calls if argv[:3] == ("sudo", "-n", "base64")]


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
        self, names: Sequence[str], gnomon: FakeGnomon, extra: Sequence[str] = ()
    ) -> tuple[int, str, str]:
        self.state_root = self.tmp / f"runs-{next(self.runs)}"
        argv = [
            "--nix-config",
            NIX_CONFIG,
            "--repo-root",
            REPO,
            "--state-root",
            str(self.state_root),
            *extra,
            *names,
        ]
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = lab.main(argv, layer=gnomon, clock=FakeClock())
        return code, out.getvalue(), err.getvalue()

    def run_traced(self, name: str, gnomon: FakeGnomon) -> None:
        """Run one experiment through Lab directly so emitted lines land in gnomon.trace."""
        self.state_root = self.tmp / f"runs-{next(self.runs)}"
        runner = lab.Lab(
            gnomon,
            FakeClock(),
            repo_root=Path(REPO),
            nix_config=Path(NIX_CONFIG),
            state_root=self.state_root,
            wol_iface="enp7s0",
            display_manager="display-manager.service",
            netconsole_port=6666,
            emit=gnomon.say,
        )
        runner.run_experiment(name)

    def single_run_dir(self, name: str) -> Path:
        found = sorted(self.state_root.glob(f"*-{name}"))
        self.assertEqual(len(found), 1, found)
        return found[0]

    def netconsole_lines(self, name: str) -> list[str]:
        return (self.single_run_dir(name) / "netconsole.log").read_text(encoding="utf-8").splitlines()

    def position(self, trace: Sequence[str], prefix: str) -> int:
        matches = [index for index, entry in enumerate(trace) if entry.startswith(prefix)]
        self.assertEqual(len(matches), 1, (prefix, trace))
        return matches[0]

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

    def test_netconsole_port_defaults_to_6666_and_follows_the_option(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        self.assertEqual(gnomon.netconsole_ports, [6666])
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon, extra=["--netconsole-port", "7777"])
        self.assertEqual(code, 0, err)
        self.assertEqual(gnomon.netconsole_ports, [7777])

    def test_bind_failure_refuses_before_any_remote_effect(self) -> None:
        gnomon = FakeGnomon(bind_fails=True)
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 1)
        self.assertIn("netconsole", err)
        self.assertEqual(gnomon.calls, [])
        self.assertEqual(gnomon.events, [])

    def test_listener_starts_before_the_first_build_and_stops_after_pass(self) -> None:
        gnomon = FakeGnomon()
        self.run_traced("baseline", gnomon)
        trace = gnomon.trace
        self.assertEqual(trace[0], "listen:start")
        self.assertLess(self.position(trace, "listen:start"), self.position(trace, "build:baseline:toplevel"))
        self.assertLess(self.position(trace, "say:PASS"), self.position(trace, "listen:stop"))
        self.assertEqual(trace[-1], "listen:stop")
        self.assertFalse(gnomon.listener.open)

    def test_listener_receives_lines_from_build_lab_boot_and_return(self) -> None:
        gnomon = FakeGnomon(
            deliveries={
                "build:baseline:toplevel": [b"build\n"],
                "reboot": [b"reboot\n"],
                "rm-entry": [b"return\n"],
            }
        )
        self.run_traced("baseline", gnomon)
        # rm-entry fires once before install and once after the return, so the
        # second "return" line exists only if the listener was still open at return.
        self.assertEqual(
            self.netconsole_lines("baseline"),
            [
                f"{RECEIVED_STAMP} build",
                f"{RECEIVED_STAMP} return",
                f"{RECEIVED_STAMP} reboot",
                f"{RECEIVED_STAMP} reboot",
                f"{RECEIVED_STAMP} return",
            ],
        )

    def test_listener_stays_open_through_the_abort_steps(self) -> None:
        gnomon = FakeGnomon(
            lab_mode="returns",
            display_manager="inactive",
            deliveries={"oneshot-clear": [b"abort\n"]},
        )
        with self.assertRaises(lab.LabError):
            self.run_traced("fault-panic", gnomon)
        trace = gnomon.trace
        self.assertLess(self.position(trace, "say:FAIL"), self.position(trace, "listen:stop"))
        self.assertLess(
            self.position(trace, "say:aborting"), self.position(trace, "oneshot-clear")
        )
        self.assertEqual(trace[-1], "listen:stop")
        self.assertEqual(self.netconsole_lines("fault-panic"), [f"{RECEIVED_STAMP} abort"])

    def test_listener_is_stopped_when_a_run_fails_before_install(self) -> None:
        gnomon = FakeGnomon(fail_gc_root="uki")
        with self.assertRaises(lab.LabError):
            self.run_traced("fault-panic", gnomon)
        self.assertLess(self.position(gnomon.trace, "say:FAIL"), self.position(gnomon.trace, "listen:stop"))
        self.assertEqual(gnomon.trace[-1], "listen:stop")
        self.assertFalse(gnomon.listener.open)

    def test_firmware_bytes_land_at_the_frozen_run_layout(self) -> None:
        fixtures = firmware_fixtures()
        gnomon = FakeGnomon(firmware=fixtures)
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        run = self.single_run_dir("baseline")
        collected = {
            path.relative_to(run).as_posix() for path in (run / "firmware").rglob("*") if path.is_file()
        }
        self.assertEqual(collected, set(FIRMWARE_LAYOUT.values()))
        for remote, relative in FIRMWARE_LAYOUT.items():
            with self.subTest(remote=remote):
                self.assertEqual((run / relative).read_bytes(), fixtures[remote])

    def test_firmware_is_copied_between_the_checked_lab_boot_and_the_return_reboot(self) -> None:
        gnomon = FakeGnomon(firmware=firmware_fixtures())
        self.run_traced("baseline", gnomon)
        trace = gnomon.trace
        reboots = [index for index, entry in enumerate(trace) if entry == "reboot"]
        self.assertEqual(len(reboots), 2, trace)
        booted = self.position(trace, "say:lab entry booted")
        copies = [index for index, entry in enumerate(trace) if entry.startswith("base64:")]
        self.assertEqual(len(copies), len(FIRMWARE_LAYOUT), trace)
        self.assertLess(reboots[0], booted)
        for index in copies:
            with self.subTest(entry=trace[index]):
                self.assertLess(booted, index)
                self.assertLess(index, reboots[1])

    def test_firmware_is_not_copied_without_a_checked_lab_boot(self) -> None:
        cases = (
            ("fault-panic", "returns", 0),
            ("fault-poweroff", "poweroff", 0),
            ("baseline", "returns", 1),
        )
        for name, lab_mode, code_expected in cases:
            with self.subTest(name=name, lab_mode=lab_mode):
                gnomon = FakeGnomon(lab_mode=lab_mode, firmware=firmware_fixtures())
                code, _, err = self.run_queue([name], gnomon)
                self.assertEqual(code, code_expected, err)
                self.assertEqual(base64_calls(gnomon), [])
                self.assertFalse((self.single_run_dir(name) / "firmware").exists())

    def test_missing_firmware_file_is_noted_and_collection_goes_on(self) -> None:
        fixtures = firmware_fixtures()
        absent = [remote for remote in fixtures if "/acpi/bgrt/" in remote]
        for remote in absent:
            del fixtures[remote]
        gnomon = FakeGnomon(firmware=fixtures)
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        run = self.single_run_dir("baseline")
        lines = (run / "run.log").read_text(encoding="utf-8").splitlines()
        for remote in absent:
            with self.subTest(remote=remote):
                self.assertTrue(
                    any(remote in line and "No such file or directory" in line for line in lines), lines
                )
                self.assertFalse((run / FIRMWARE_LAYOUT[remote]).exists())
        self.assertIn("PASS", lines)
        for remote, relative in FIRMWARE_LAYOUT.items():
            if remote in fixtures:
                with self.subTest(remote=remote):
                    self.assertEqual((run / relative).read_bytes(), fixtures[remote])

    def test_undecodable_firmware_output_is_noted_and_skipped(self) -> None:
        garbled = "/sys/firmware/acpi/fpdt/boot/bootloader_load_ns"
        gnomon = FakeGnomon(firmware=firmware_fixtures(), firmware_garbled=[garbled])
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        run = self.single_run_dir("baseline")
        lines = (run / "run.log").read_text(encoding="utf-8").splitlines()
        self.assertTrue(any(garbled in line and "undecodable" in line for line in lines), lines)
        self.assertFalse((run / FIRMWARE_LAYOUT[garbled]).exists())
        self.assertEqual(
            (run / FIRMWARE_LAYOUT["/sys/kernel/boot_params/data"]).read_bytes(),
            firmware_fixtures()["/sys/kernel/boot_params/data"],
        )
        self.assertIn("PASS", lines)

    def test_checked_lab_boot_journal_is_saved_byte_for_byte(self) -> None:
        gnomon = FakeGnomon()
        code, _, err = self.run_queue(["baseline"], gnomon)
        self.assertEqual(code, 0, err)
        saved = (self.single_run_dir("baseline") / "journal.json").read_bytes()
        self.assertIsNotNone(gnomon.lab_boot)
        self.assertEqual(saved, gnomon.persisted[gnomon.lab_boot].encode("utf-8"))
        self.assertNotEqual(saved, gnomon.persisted[boot_uuid("boot-1")].encode("utf-8"))
        self.assertNotEqual(saved, gnomon.persisted[gnomon.current_boot].encode("utf-8"))

    def test_unchecked_lab_boot_journal_is_found_through_list_boots(self) -> None:
        for name in ("fault-initrd", "fault-hang", "fault-panic", "fault-poweroff"):
            with self.subTest(name=name):
                lab_mode = "poweroff" if name == "fault-poweroff" else "returns"
                gnomon = FakeGnomon(lab_mode=lab_mode)
                code, _, err = self.run_queue([name], gnomon)
                self.assertEqual(code, 0, err)
                saved = (self.single_run_dir(name) / "journal.json").read_bytes()
                self.assertIsNotNone(gnomon.lab_boot)
                self.assertEqual(saved, gnomon.persisted[gnomon.lab_boot].encode("utf-8"))
                self.assertNotEqual(saved, gnomon.persisted[gnomon.current_boot].encode("utf-8"))
                self.assertNotEqual(saved, gnomon.persisted[boot_uuid("older-2")].encode("utf-8"))

    def test_lab_boot_without_journal_is_noted_and_outcome_unchanged(self) -> None:
        cases = (
            ("baseline", "boots", "is not available"),
            ("fault-panic", "returns", "left no journal"),
        )
        for name, lab_mode, reason in cases:
            with self.subTest(name=name):
                gnomon = FakeGnomon(lab_mode=lab_mode, lab_journal=False)
                code, _, err = self.run_queue([name], gnomon)
                self.assertEqual(code, 0, err)
                run = self.single_run_dir(name)
                lines = (run / "run.log").read_text(encoding="utf-8").splitlines()
                self.assertIn("PASS", lines)
                self.assertFalse((run / "journal.json").exists())
                noted = [
                    line for line in lines if line.startswith("journal:") and "journal.json not written" in line
                ]
                self.assertTrue(any(reason in line for line in noted), lines)

    def test_journal_is_collected_only_after_gnomon_returns(self) -> None:
        gnomon = FakeGnomon()
        self.run_traced("baseline", gnomon)
        returned = self.position(gnomon.trace, "say:gnomon returned to")
        reads = [index for index, entry in enumerate(gnomon.trace) if entry.startswith("journal:")]
        self.assertEqual(len(reads), 1, gnomon.trace)
        self.assertGreater(reads[0], returned)
        gnomon = FakeGnomon(lab_mode="returns")
        self.run_traced("fault-panic", gnomon)
        returned = self.position(gnomon.trace, "say:gnomon returned to")
        reads = [index for index, entry in enumerate(gnomon.trace) if entry.startswith("journal:")]
        self.assertEqual(len(reads), 2, gnomon.trace)
        for index in reads:
            with self.subTest(entry=gnomon.trace[index]):
                self.assertGreater(index, returned)

    def test_journal_listing_failure_is_noted_and_outcome_unchanged(self) -> None:
        gnomon = FakeGnomon(lab_mode="returns", list_boots_fails=True)
        code, _, err = self.run_queue(["fault-panic"], gnomon)
        self.assertEqual(code, 0, err)
        run = self.single_run_dir("fault-panic")
        lines = (run / "run.log").read_text(encoding="utf-8").splitlines()
        self.assertIn("PASS", lines)
        self.assertTrue(any("list-boots failed" in line for line in lines), lines)
        self.assertFalse((run / "journal.json").exists())


class NetconsoleLogTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = Path(tmp.name) / "netconsole.log"

    def write(self, *datagrams: bytes) -> list[str]:
        log = lab.NetconsoleLog(self.path)
        try:
            for datagram in datagrams:
                log.record(RECEIVED, datagram)
        finally:
            log.close()
        return self.path.read_text(encoding="utf-8").splitlines()

    def test_each_line_is_prefixed_with_the_receive_time(self) -> None:
        lines = self.write(b"6,1234,5678901,-;nvidia-drm 0000:01:00.0: probe\n")
        self.assertEqual(lines, [f"{RECEIVED_STAMP} 6,1234,5678901,-;nvidia-drm 0000:01:00.0: probe"])

    def test_datagram_lines_split_into_one_line_each(self) -> None:
        lines = self.write(b"6,1,1,-;first\n6,2,2,-;second\n")
        self.assertEqual(lines, [f"{RECEIVED_STAMP} 6,1,1,-;first", f"{RECEIVED_STAMP} 6,2,2,-;second"])

    def test_last_line_without_newline_is_kept(self) -> None:
        self.assertEqual(self.write(b"a\nb"), [f"{RECEIVED_STAMP} a", f"{RECEIVED_STAMP} b"])

    def test_empty_datagram_writes_nothing(self) -> None:
        self.assertEqual(self.write(b""), [])

    def test_undecodable_bytes_are_replaced(self) -> None:
        self.assertEqual(self.write(b"x\xffy\n"), [f"{RECEIVED_STAMP} x�y"])

    def test_each_datagram_is_flushed_before_the_next_arrives(self) -> None:
        log = lab.NetconsoleLog(self.path)
        self.addCleanup(log.close)
        log.record(RECEIVED, b"kept\n")
        self.assertEqual(self.path.read_text(encoding="utf-8"), f"{RECEIVED_STAMP} kept\n")


class NetconsoleListenerTest(unittest.TestCase):
    def free_port(self) -> int:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind(("", 0))
            return sock.getsockname()[1]

    def test_loopback_datagram_reaches_the_callback_until_stopped(self) -> None:
        port = self.free_port()
        received: list[tuple[datetime, bytes]] = []
        arrived = threading.Event()

        def on_datagram(at: datetime, payload: bytes) -> None:
            received.append((at, payload))
            arrived.set()

        before = datetime.now(timezone.utc)
        listener = lab.SystemLayer().netconsole(port, on_datagram)
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                sender.sendto(b"6,1,1,-;hello\n", ("127.0.0.1", port))
            self.assertTrue(arrived.wait(timeout=10))
        finally:
            listener.stop()
        after = datetime.now(timezone.utc)
        at, payload = received[0]
        self.assertEqual(payload, b"6,1,1,-;hello\n")
        self.assertEqual(at.utcoffset(), timedelta(0))
        self.assertLessEqual(before, at)
        self.assertLessEqual(at, after)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as rebind:
            rebind.bind(("", port))

    def test_port_already_in_use_is_a_lab_error(self) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as held:
            held.bind(("", 0))
            port = held.getsockname()[1]
            with self.assertRaises(lab.LabError) as caught:
                lab.SystemLayer().netconsole(port, lambda at, payload: None)
        self.assertIn(f"UDP port {port}", str(caught.exception))


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


class BootListParseTest(unittest.TestCase):
    FIRST = "0123456789abcdef0123456789abcdef"
    SECOND = "fedcba9876543210fedcba9876543210"

    def test_boot_ids_come_back_in_listing_order_as_printed(self) -> None:
        listing = json.dumps(
            [
                {"index": -1, "boot_id": self.FIRST, "first_entry": 1, "last_entry": 2},
                {"index": 0, "boot_id": self.SECOND, "first_entry": 3, "last_entry": 4},
            ]
        )
        self.assertEqual(lab.parse_boot_list(listing), [self.FIRST, self.SECOND])

    def test_text_without_json_is_refused(self) -> None:
        with self.assertRaises(lab.LabError):
            lab.parse_boot_list("Failed to list boots\n")

    def test_records_without_boot_ids_are_refused(self) -> None:
        for listing in (json.dumps({"boot_id": self.FIRST}), json.dumps([{"index": 0}])):
            with self.subTest(listing=listing):
                with self.assertRaises(lab.LabError):
                    lab.parse_boot_list(listing)


if __name__ == "__main__":
    unittest.main()
