#!/usr/bin/env python3
"""Vermissian-side lab runner: boots gnomon into lab UKIs and back.

Per experiment: realise the lab toplevel and UKI from nix-config's `labSystems`
output in gnomon's store (evaluated here, built by gnomon), list the UKI image
on gnomon, sign it with the sbctl db key into a staging path, check the ESP has
room, record the boot entries, default entry, and system generations, install
the UKI as a one-shot ESP entry, reboot, and verify gnomon returns to the
pre-run default with the pre-run record intact.
Before anything is staged, the run points two GC roots on gnomon at the built
images: /nix/var/nix/gcroots/halmasuit-lab/toplevel at the lab toplevel and
/nix/var/nix/gcroots/halmasuit-lab/uki at the lab UKI. Each run replaces existing
links, and the runner never removes them, so a failed or aborted run leaves them
in place. The roots keep the images in gnomon's store until someone removes them.
They are not system generations and create no boot entries.
The lab entry is removed on the way out.
When an experiment checks its lab boot, the runner copies gnomon's firmware and
loader evidence (FPDT, BGRT, boot_params, and the systemd-boot efivars) into the
run's firmware/ directory while the lab entry is up; a file gnomon lacks is noted in
run.log and skipped.
The baseline experiment also probes the greeter on the checked lab boot, once the display
manager is active. The probe refuses, and never starts lab-probe.service, when the resolved
greetd PAM stack holds a lockout module (pam_faillock, pam_tally, pam_tally2) or cannot be
resolved. Otherwise the unit types a wrong password into greetd, and the probe passes when the
lab boot's authpriv journal then holds a pam_unix(greetd:auth) authentication failure for
--probe-user within 60 seconds. probe.json records the outcome. A refused or failed probe fails
the run after gnomon has returned to its default entry.
Once gnomon has returned, the runner saves the lab boot's journal as journal.json in the
run directory (`journalctl -b ID -o json`). A checked lab boot is known from the boot the
runner saw; otherwise the lab boot is the boot list-boots lists right after the boot the
run started from. A lab boot with no journal, or a journal gnomon cannot give, is noted in
run.log and no journal.json is written; neither changes the run's outcome.

Every remote, local, clock, UDP, and magic-packet effect goes through the `Layer`
and `Clock` objects handed to `Lab`, so test_lab.py runs the whole sequence
against a simulated gnomon.

From before the first build until PASS, FAIL, or the abort has finished, the
runner listens for gnomon's netconsole stream on UDP. Every received line goes
to the run's netconsole.log, prefixed with the time vermissian received it.

A run refuses before it installs anything, and again before each reboot it
issues, while a graphical user session (Class=user, Type wayland, x11, or mir)
is logged in. Any failure after the install removes the lab entry and clears
the one-shot before the runner exits.
"""

from __future__ import annotations

import argparse
import base64
import json
import re
import shlex
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple, Protocol, TypeVar

import pam_stack

T = TypeVar("T")
REPO_ROOT = Path(__file__).resolve().parents[2]
GNOMON = "gnomon"
LAB_ENTRY = "halmasuit-lab.efi"
ESP_ENTRY = f"/boot/EFI/Linux/{LAB_ENTRY}"
STAGED_DIR = "/var/lib/halmasuit-lab"
STAGED = f"{STAGED_DIR}/{LAB_ENTRY}"
GC_ROOT_DIR = "/nix/var/nix/gcroots/halmasuit-lab"
GC_TOPLEVEL = f"{GC_ROOT_DIR}/toplevel"
GC_UKI = f"{GC_ROOT_DIR}/uki"
BOOT_ID_FILE = "/proc/sys/kernel/random/boot_id"
SELECTED_VAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
LOADER_GUID = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
FIRMWARE_ROOT = "/sys/firmware/"
BOOT_PARAMS_DATA = "/sys/kernel/boot_params/data"
# Copied from gnomon while the lab entry is up; the run-directory layout is frozen (E1-D6).
FIRMWARE_FILES = (
    "/sys/firmware/acpi/tables/FPDT",
    "/sys/firmware/acpi/fpdt/boot/firmware_start_ns",
    "/sys/firmware/acpi/fpdt/boot/bootloader_load_ns",
    "/sys/firmware/acpi/fpdt/boot/bootloader_launch_ns",
    "/sys/firmware/acpi/fpdt/boot/exitbootservice_start_ns",
    "/sys/firmware/acpi/fpdt/boot/exitbootservice_end_ns",
    "/sys/firmware/acpi/bgrt/status",
    "/sys/firmware/acpi/bgrt/type",
    "/sys/firmware/acpi/bgrt/version",
    "/sys/firmware/acpi/bgrt/xoffset",
    "/sys/firmware/acpi/bgrt/yoffset",
    "/sys/firmware/acpi/bgrt/image",
    BOOT_PARAMS_DATA,
    f"/sys/firmware/efi/efivars/LoaderTimeInitUSec-{LOADER_GUID}",
    f"/sys/firmware/efi/efivars/LoaderTimeExecUSec-{LOADER_GUID}",
    f"/sys/firmware/efi/efivars/LoaderFirmwareInfo-{LOADER_GUID}",
)
ESP_MARGIN_BYTES = 200 * 1024 * 1024
GRAPHICAL_TYPES = frozenset({"wayland", "x11", "mir"})
MAC_PATTERN = re.compile(r"[0-9a-f]{2}(:[0-9a-f]{2}){5}")
POLL_SECONDS = 5
BOOT_DEADLINE_SECONDS = 900
SHUTDOWN_DEADLINE_SECONDS = 600
DISPLAY_DEADLINE_SECONDS = 300
REMOTE_TIMEOUT_SECONDS = 120
MAGIC_PORT = 9
NETCONSOLE_PORT = 6666
NETCONSOLE_LOG = "netconsole.log"
UDP_MAX_DATAGRAM = 65535
LISTEN_POLL_SECONDS = 0.2
PAM_DIR = "/etc/pam.d/"
PAM_SERVICE = "greetd"
PROBE_UNIT = "lab-probe.service"
PROBE_MARKER = f"pam_unix({PAM_SERVICE}:auth): authentication failure"
PROBE_DEADLINE_SECONDS = 60
AUTHPRIV_FACILITY = 10
DEFAULT_PROBE_USER = "joshsymonds"


class LabError(Exception):
    """A lab step failed or refused; the queue stops at the first one."""


class Result(NamedTuple):
    returncode: int
    stdout: str
    stderr: str


DatagramSink = Callable[[datetime, bytes], None]


class NetconsoleListener(Protocol):
    def stop(self) -> None: ...


class Layer(Protocol):
    """Every effect the runner has on the outside world."""

    def local(self, argv: Sequence[str]) -> Result: ...

    def remote(self, argv: Sequence[str]) -> Result: ...

    def wake(self, mac: str) -> None: ...

    def netconsole(self, port: int, on_datagram: DatagramSink) -> NetconsoleListener: ...


class Clock(Protocol):
    def monotonic(self) -> float: ...

    def sleep(self, seconds: float) -> None: ...

    def stamp(self) -> str: ...


class SystemLayer:
    """The real effects: local processes, ssh to the gnomon alias, UDP broadcast and listen."""

    def local(self, argv: Sequence[str]) -> Result:
        return _capture(list(argv), timeout=None)

    def remote(self, argv: Sequence[str]) -> Result:
        # ssh hands its remote side to gnomon's shell as one string; shlex.join
        # quotes each word for that shell.
        return _capture(
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", GNOMON, shlex.join(argv)],
            timeout=REMOTE_TIMEOUT_SECONDS,
        )

    def wake(self, mac: str) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
            sock.sendto(magic_packet(mac), ("255.255.255.255", MAGIC_PORT))

    def netconsole(self, port: int, on_datagram: DatagramSink) -> NetconsoleListener:
        return UdpListener(_bind_udp(port), on_datagram)


def _bind_udp(port: int) -> socket.socket:
    """A UDP socket on every IPv4 address at `port`, polled so a stop() never blocks long."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.bind(("", port))
    except OSError as err:
        sock.close()
        raise LabError(f"cannot listen for netconsole on UDP port {port}: {err}") from err
    sock.settimeout(LISTEN_POLL_SECONDS)
    return sock


class UdpListener:
    """Receives datagrams on a background thread until stop().

    A failure on the thread (a closed socket or a callback error) is raised by stop(),
    so a broken listener fails the run instead of silently dropping kernel log lines.
    """

    def __init__(self, sock: socket.socket, on_datagram: DatagramSink) -> None:
        self._sock = sock
        self._on_datagram = on_datagram
        self._stopping = threading.Event()
        self._failure: Exception | None = None
        self._thread = threading.Thread(target=self._receive, name="netconsole", daemon=True)
        self._thread.start()

    def _receive(self) -> None:
        try:
            while not self._stopping.is_set():
                try:
                    datagram, _ = self._sock.recvfrom(UDP_MAX_DATAGRAM)
                except TimeoutError:
                    continue
                self._on_datagram(datetime.now(timezone.utc), datagram)
        except Exception as err:
            self._failure = err

    def stop(self) -> None:
        self._stopping.set()
        self._thread.join()
        self._sock.close()
        if self._failure is not None:
            raise LabError(f"netconsole listener failed: {self._failure}") from self._failure


class NetconsoleLog:
    """A run's netconsole.log: every received line, prefixed with its receive time."""

    def __init__(self, path: Path) -> None:
        self._file = path.open("a", encoding="utf-8")

    def record(self, received: datetime, datagram: bytes) -> None:
        stamp = received.isoformat(timespec="microseconds")
        lines = datagram.split(b"\n")
        if lines[-1] == b"":
            lines.pop()
        for line in lines:
            text = line.decode("utf-8", errors="replace")
            self._file.write(f"{stamp} {text}\n")
        self._file.flush()

    def close(self) -> None:
        self._file.close()


class SystemClock:
    def monotonic(self) -> float:
        return time.monotonic()

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)

    def stamp(self) -> str:
        return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _capture(argv: list[str], timeout: float | None) -> Result:
    try:
        done = subprocess.run(argv, capture_output=True, text=True, check=False, timeout=timeout)
    except subprocess.TimeoutExpired:
        return Result(124, "", f"timed out after {timeout} seconds")
    return Result(done.returncode, done.stdout, done.stderr)


@dataclass(frozen=True)
class Plan:
    """What an experiment expects gnomon to do after the reboot into the lab entry."""

    lab_boot_checked: bool  # the lab entry must be seen booting before the return reboot
    poweroff: bool  # gnomon powers off and returns on wake-on-LAN
    probe: bool  # the greeter's PAM stack is probed with a wrong password on the checked lab boot


EXPERIMENTS: dict[str, Plan] = {
    "baseline": Plan(lab_boot_checked=True, poweroff=False, probe=True),
    "fault-initrd": Plan(lab_boot_checked=False, poweroff=False, probe=False),
    "fault-hang": Plan(lab_boot_checked=False, poweroff=False, probe=False),
    "fault-panic": Plan(lab_boot_checked=False, poweroff=False, probe=False),
    "fault-poweroff": Plan(lab_boot_checked=False, poweroff=True, probe=False),
}


@dataclass(frozen=True)
class Record:
    """The boot state gnomon must still show after a run."""

    entries: tuple[str, ...]
    default: str
    generations: tuple[int, ...]


def magic_packet(mac: str) -> bytes:
    """The Wake-on-LAN payload: six 0xff bytes, then the MAC sixteen times."""
    return b"\xff" * 6 + bytes.fromhex(mac.replace(":", "")) * 16


def parse_bootctl_list(listing: str) -> tuple[list[str], str]:
    """Entry ids in listing order, and the one entry bootctl marks as default.

    Reads `bootctl list --json=short`. isDefault names the entry the loader
    boots next, so a one-shot entry is the one marked while a one-shot is set.
    Entries are the type1/type2 records on disk; `loader` records (reported by
    the boot loader at this boot, absent on disk) and `auto` records are not.
    """
    try:
        records = json.loads(listing)
    except json.JSONDecodeError as err:
        raise LabError(f"bootctl list printed no JSON: {err}") from err
    if not isinstance(records, list) or not all(
        isinstance(record, dict) and isinstance(record.get("id"), str) for record in records
    ):
        raise LabError("bootctl list JSON is not a list of entries with ids")
    entries = [record["id"] for record in records if record.get("type") in ("type1", "type2")]
    defaults = [record["id"] for record in records if record.get("isDefault") is True]
    if len(defaults) != 1:
        raise LabError(f"bootctl list names {len(defaults)} default entries, expected exactly one")
    return entries, defaults[0]


def parse_boot_list(listing: str) -> list[str]:
    """Boot ids in chronological order, as `journalctl --list-boots -o json` prints them."""
    try:
        records = json.loads(listing)
    except json.JSONDecodeError as err:
        raise LabError(f"journalctl --list-boots printed no JSON: {err}") from err
    if not isinstance(records, list) or not all(
        isinstance(record, dict) and isinstance(record.get("boot_id"), str) for record in records
    ):
        raise LabError("journalctl --list-boots JSON is not a list of boots with ids")
    return [record["boot_id"] for record in records]


def _boot_key(boot: str) -> str:
    """A boot id with its dashes removed and lowercased, so list-boots and /proc ids compare equal."""
    return boot.replace("-", "").lower()


def parse_generations(profiles: str) -> tuple[int, ...]:
    numbers = []
    for name in profiles.split():
        match = re.fullmatch(r"system-(\d+)-link", name)
        if match:
            numbers.append(int(match.group(1)))
    return tuple(sorted(numbers))


def _firmware_destination(remote: str) -> Path:
    """Where a firmware file lands in the run directory: firmware/<path under /sys/firmware>."""
    if remote == BOOT_PARAMS_DATA:
        return Path("firmware/boot_params/data")
    return Path("firmware") / remote.removeprefix(FIRMWARE_ROOT)


def _int(text: str, what: str) -> int:
    try:
        return int(text)
    except ValueError as err:
        raise LabError(f"could not read {what} from {text!r}") from err


def _last_line(text: str) -> str:
    lines = [line for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else ""


def _ok(result: Result, what: str) -> str:
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise LabError(f"{what} failed (rc {result.returncode}): {detail}")
    return result.stdout


def _compare(pre: Record, post: Record) -> None:
    mismatches = []
    if post.entries != pre.entries:
        mismatches.append(f"entries {list(pre.entries)} became {list(post.entries)}")
    if post.default != pre.default:
        mismatches.append(f"default {pre.default} became {post.default}")
    if post.generations != pre.generations:
        mismatches.append(f"generations {list(pre.generations)} became {list(post.generations)}")
    if mismatches:
        raise LabError("post-run record mismatch: " + "; ".join(mismatches))


class RunLog:
    """One directory per experiment run: a readable log and the pre-run record."""

    def __init__(self, root: Path, stamp: str, name: str, emit: Callable[[str], None]) -> None:
        self.directory = root / f"{stamp}-{name}"
        self.directory.mkdir(parents=True, exist_ok=False)
        self._emit = emit

    def say(self, message: str) -> None:
        self._emit(message)
        with (self.directory / "run.log").open("a", encoding="utf-8") as log:
            log.write(message + "\n")

    def save(self, filename: str, data: dict) -> None:
        (self.directory / filename).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


class Lab:
    def __init__(
        self,
        layer: Layer,
        clock: Clock,
        *,
        repo_root: Path,
        nix_config: Path,
        state_root: Path,
        wol_iface: str,
        display_manager: str,
        netconsole_port: int,
        probe_user: str,
        emit: Callable[[str], None],
    ) -> None:
        self.layer = layer
        self.clock = clock
        self.repo_root = repo_root
        self.nix_config = nix_config
        self.state_root = state_root
        self.wol_iface = wol_iface
        self.display_manager = display_manager
        self.netconsole_port = netconsole_port
        self.probe_user = probe_user
        self.emit = emit

    def run_experiment(self, name: str) -> None:
        plan = EXPERIMENTS[name]
        run = RunLog(self.state_root, self.clock.stamp(), name, self.emit)
        netconsole = NetconsoleLog(run.directory / NETCONSOLE_LOG)
        listener: NetconsoleListener | None = None
        installed = False
        try:
            listener = self.layer.netconsole(self.netconsole_port, netconsole.record)
            head = self._head_rev()
            run.say(f"experiment {name} from halmasuit {head}")
            toplevel = self._build(name, "toplevel", head)
            uki = self._build(name, "uki", head)
            self._point_gc_roots(toplevel, uki)
            image = self._uki_image(uki)
            self._stage(image)
            self._remove_entry()
            self._check_esp_room(image)
            listing, pre = self._record()
            run.say(f"pre-run bootctl list:\n{listing.rstrip()}")
            boot_before = self._require_boot_id()
            mac = self._read_mac() if plan.poweroff else None
            run.save(
                "record.json",
                {
                    "boot_before": boot_before,
                    "entries": list(pre.entries),
                    "default": pre.default,
                    "generations": list(pre.generations),
                    "wol_mac": mac,
                },
            )
            self._refuse_if_graphical("refusing before install")
            installed = True
            self._install(pre)
            run.say(f"installed {LAB_ENTRY} as the one-shot")
            self._refuse_if_graphical("refusing before reboot")
            self._reboot()
            booted_from = boot_before
            lab_boot: str | None = None
            probe: dict | None = None
            if plan.lab_boot_checked:
                lab_boot = self._await_boot("lab entry to boot", boot_before, LAB_ENTRY)
                booted_from = lab_boot
                run.say(f"lab entry booted as {lab_boot}")
                self._collect_firmware(run)
                if plan.probe:
                    probe = self._probe(run)
                    run.save("probe.json", probe)
                self._refuse_if_graphical("refusing before return reboot")
                self._reboot()
            if plan.poweroff:
                self._await_power_off()
                run.say("gnomon powered off; sending wake-on-LAN")
                returned = self._await_boot(
                    "default entry after wake-on-LAN", booted_from, pre.default, wake_mac=mac
                )
            else:
                returned = self._await_boot("default entry to return", booted_from, pre.default)
            run.say(f"gnomon returned to {pre.default} as {returned}")
            self._collect_journal(run, boot_before, lab_boot, returned)
            self._await_display_manager()
            self._remove_entry()
            _, post = self._record()
            _compare(pre, post)
            if probe is not None:
                self._check_probe(probe)
            run.say("PASS")
        except BaseException as err:
            run.say(f"FAIL: {err}")
            if installed:
                self._abort(run)
            raise
        finally:
            if listener is not None:
                listener.stop()
            netconsole.close()

    def _head_rev(self) -> str:
        return _ok(
            self.layer.local(["git", "-C", str(self.repo_root), "rev-parse", "HEAD"]),
            "git rev-parse HEAD",
        ).strip()

    def _build(self, name: str, kind: str, head: str) -> str:
        installable = f"{self.nix_config}#labSystems.gnomon.{name}.config.system.build.{kind}"
        out = _ok(
            self.layer.local(
                [
                    "nix",
                    "build",
                    "--eval-store",
                    "auto",
                    "--store",
                    f"ssh-ng://{GNOMON}",
                    "--no-link",
                    "--print-out-paths",
                    installable,
                    "--override-input",
                    "halmasuit",
                    f"git+file://{self.repo_root}?rev={head}",
                ]
            ),
            f"nix build {name} {kind}",
        ).strip()
        if not out.startswith("/nix/store/"):
            raise LabError(f"nix build {name} {kind} printed no store path: {out!r}")
        return out

    def _point_gc_roots(self, toplevel: str, uki: str) -> None:
        _ok(
            self.layer.remote(["sudo", "-n", "install", "-d", "-m", "0755", GC_ROOT_DIR]),
            "create GC root directory",
        )
        _ok(
            self.layer.remote(["sudo", "-n", "ln", "-sfnT", toplevel, GC_TOPLEVEL]),
            "point toplevel GC root",
        )
        _ok(self.layer.remote(["sudo", "-n", "ln", "-sfnT", uki, GC_UKI]), "point uki GC root")

    def _uki_image(self, uki: str) -> str:
        """The one .efi image inside the UKI store directory."""
        names = _ok(self.layer.remote(["ls", "-1", uki]), f"list {uki}").split()
        images = [name for name in names if name.endswith(".efi")]
        if len(images) != 1:
            raise LabError(f"{uki} holds {len(images)} .efi images, expected exactly one")
        return f"{uki}/{images[0]}"

    def _stage(self, image: str) -> None:
        _ok(
            self.layer.remote(["sudo", "-n", "install", "-d", "-m", "0700", STAGED_DIR]),
            "create staging directory",
        )
        _ok(
            self.layer.remote(["sudo", "-n", "sbctl", "sign", "-o", STAGED, image]),
            "sbctl sign",
        )

    def _remove_entry(self) -> None:
        _ok(self.layer.remote(["sudo", "-n", "rm", "-f", ESP_ENTRY]), "remove lab entry")

    def _check_esp_room(self, image: str) -> None:
        free = _int(
            _last_line(_ok(self.layer.remote(["df", "-B1", "--output=avail", "/boot"]), "df /boot")),
            "ESP free bytes",
        )
        size = _int(_ok(self.layer.remote(["stat", "-c", "%s", image]), "stat UKI image").strip(), "UKI size")
        need = size + ESP_MARGIN_BYTES
        if free < need:
            raise LabError(f"ESP has {free} bytes free; need {need} ({size} image + 200 MiB margin)")

    def _bootctl_listing(self) -> str:
        return _ok(
            self.layer.remote(["sudo", "-n", "bootctl", "list", "--json=short"]), "bootctl list"
        )

    def _record(self) -> tuple[str, Record]:
        listing = self._bootctl_listing()
        entries, default = parse_bootctl_list(listing)
        profiles = _ok(self.layer.remote(["ls", "-1", "/nix/var/nix/profiles"]), "list system profiles")
        return listing, Record(tuple(entries), default, parse_generations(profiles))

    def _install(self, pre: Record) -> None:
        _ok(self.layer.remote(["sudo", "-n", "cp", STAGED, ESP_ENTRY]), "install lab entry")
        # Checked before set-oneshot: once a one-shot is set, bootctl names it as default.
        _, default = parse_bootctl_list(self._bootctl_listing())
        if default != pre.default:
            raise LabError(
                f"bootctl names {default} as default after install; "
                f"pre-run default was {pre.default}"
            )
        _ok(self.layer.remote(["sudo", "-n", "bootctl", "set-oneshot", LAB_ENTRY]), "set one-shot")

    def _refuse_if_graphical(self, when: str) -> None:
        listing = _ok(
            self.layer.remote(["loginctl", "list-sessions", "--no-legend"]),
            "loginctl list-sessions",
        )
        for line in listing.splitlines():
            fields = line.split()
            if not fields:
                continue
            session = fields[0]
            props = _ok(
                self.layer.remote(["loginctl", "show-session", session, "-p", "Class", "-p", "Type"]),
                f"loginctl show-session {session}",
            )
            values: dict[str, str] = {}
            for prop in props.splitlines():
                key, _, value = prop.partition("=")
                values[key] = value
            if values.get("Class") == "user" and values.get("Type") in GRAPHICAL_TYPES:
                raise LabError(
                    f"{when}: session {session} is a logged-in {values['Type']} user session"
                )

    def _reboot(self) -> None:
        result = self.layer.remote(["sudo", "-n", "systemctl", "reboot"])
        # ssh exits 255 when the connection drops as the reboot begins.
        if result.returncode not in (0, 255):
            raise LabError(f"reboot failed (rc {result.returncode}): {result.stderr.strip()}")

    def _boot_id(self) -> str | None:
        result = self.layer.remote(["cat", BOOT_ID_FILE])
        boot = result.stdout.strip()
        return boot if result.returncode == 0 and boot else None

    def _require_boot_id(self) -> str:
        boot = self._boot_id()
        if boot is None:
            raise LabError("gnomon is not reachable over ssh before the run")
        return boot

    def _selected(self) -> str | None:
        selected_cmd = f"tail -c +5 {SELECTED_VAR} | iconv -f UTF-16LE -t UTF-8"
        result = self.layer.remote(["sudo", "-n", "sh", "-c", selected_cmd])
        return result.stdout.strip("\x00 \t\r\n") if result.returncode == 0 else None

    def _read_mac(self) -> str:
        mac = _ok(
            self.layer.remote(["cat", f"/sys/class/net/{self.wol_iface}/address"]),
            f"read MAC of {self.wol_iface}",
        ).strip()
        if not MAC_PATTERN.fullmatch(mac):
            raise LabError(f"unexpected MAC {mac!r} for {self.wol_iface}")
        return mac

    def _poll(self, what: str, check: Callable[[], T | None], deadline: float) -> T:
        started = self.clock.monotonic()
        while True:
            value = check()
            if value is not None:
                return value
            if self.clock.monotonic() - started >= deadline:
                raise LabError(f"timed out after {deadline:.0f} seconds waiting for {what}")
            self.clock.sleep(POLL_SECONDS)

    def _await_boot(self, what: str, before: str, entry: str, wake_mac: str | None = None) -> str:
        """Wait for a boot other than `before` that selected `entry`; return its boot id.

        With wake_mac set, a magic packet is sent on every poll while gnomon is unreachable.
        """

        def booted() -> str | None:
            boot = self._boot_id()
            if boot is None:
                if wake_mac is not None:
                    self.layer.wake(wake_mac)
                return None
            if boot != before and self._selected() == entry:
                return boot
            return None

        return self._poll(what, booted, BOOT_DEADLINE_SECONDS)

    def _await_power_off(self) -> None:
        self._poll(
            "gnomon to power off",
            lambda: True if self._boot_id() is None else None,
            SHUTDOWN_DEADLINE_SECONDS,
        )

    def _await_display_manager(self) -> None:
        def active() -> bool | None:
            result = self.layer.remote(["systemctl", "is-active", self.display_manager])
            return True if result.returncode == 0 and result.stdout.strip() == "active" else None

        self._poll(f"{self.display_manager} to become active", active, DISPLAY_DEADLINE_SECONDS)

    def _probe(self, run: RunLog) -> dict:
        """Type a wrong password into greetd on the lab boot and record what the journal shows."""
        probe: dict = {
            "service": PAM_SERVICE,
            "pam_files": None,
            "pam_modules": None,
            "lockout": None,
            "refused": None,
            "started": False,
            "unit_returncode": None,
            "matched": None,
            "passed": False,
        }
        try:
            self._await_display_manager()
        except LabError as err:
            probe["refused"] = f"greeter not active: {err}"
            return probe
        try:
            stack = pam_stack.resolve(PAM_SERVICE, self._read_pam_file)
        except pam_stack.PamStackError as err:
            probe["refused"] = f"cannot resolve the {PAM_SERVICE} PAM stack: {err}"
            return probe
        probe["pam_files"] = list(stack.files)
        probe["pam_modules"] = list(stack.modules)
        probe["lockout"] = list(stack.lockout)
        if stack.lockout:
            probe["refused"] = f"{PAM_SERVICE} PAM stack holds lockout module(s): {', '.join(stack.lockout)}"
            return probe
        probe["started"] = True
        start = self.layer.remote(["sudo", "-n", "systemctl", "start", PROBE_UNIT])
        probe["unit_returncode"] = start.returncode
        try:
            probe["matched"] = self._poll(
                f"{PROBE_MARKER} for {self.probe_user} in the lab boot journal",
                self._probe_line,
                PROBE_DEADLINE_SECONDS,
            )
        except LabError as err:
            run.say(f"probe: {err}")
            return probe
        probe["passed"] = True
        return probe

    def _read_pam_file(self, name: str) -> str | None:
        """One /etc/pam.d file on gnomon, or None when it does not exist; any other failure raises."""
        result = self.layer.remote(["cat", f"{PAM_DIR}{name}"])
        if result.returncode == 0:
            return result.stdout
        if result.returncode == 1 and "No such file or directory" in result.stderr:
            return None
        detail = result.stderr.strip() or "no output"
        raise pam_stack.PamStackError(f"cannot read {PAM_DIR}{name} (rc {result.returncode}): {detail}")

    def _probe_line(self) -> str | None:
        """The lab boot's authpriv line for the probe user's greetd failure, or None while it is absent."""
        journal = _ok(
            self.layer.remote(
                [
                    "sudo",
                    "-n",
                    "journalctl",
                    "-b",
                    "0",
                    "-o",
                    "cat",
                    "--no-pager",
                    f"SYSLOG_FACILITY={AUTHPRIV_FACILITY}",
                ]
            ),
            "read the lab boot's authpriv journal",
        )
        token = f"user={self.probe_user}"
        for line in journal.splitlines():
            if PROBE_MARKER in line and token in line.split():
                return line
        return None

    def _check_probe(self, probe: dict) -> None:
        if probe["refused"] is not None:
            raise LabError(f"probe refused: {probe['refused']}")
        if not probe["passed"]:
            raise LabError(
                f"probe did not pass: no '{PROBE_MARKER}' line for {self.probe_user} in the lab boot journal"
            )

    def _collect_firmware(self, run: RunLog) -> None:
        """Copy each firmware file byte for byte; one that gnomon cannot give is noted and skipped."""
        for remote in FIRMWARE_FILES:
            result = self.layer.remote(["sudo", "-n", "base64", "-w0", remote])
            if result.returncode != 0:
                detail = result.stderr.strip() or "no output"
                run.say(f"firmware: {remote} unavailable: {detail}")
                continue
            try:
                # validate=True: a character outside the base64 alphabet is undecodable, not dropped.
                data = base64.b64decode(result.stdout, validate=True)
            except ValueError as err:
                run.say(f"firmware: {remote} unavailable: undecodable base64 output ({err})")
                continue
            destination = run.directory / _firmware_destination(remote)
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)

    def _collect_journal(self, run: RunLog, before: str, lab_boot: str | None, returned: str) -> None:
        """Save the lab boot's journal as journal.json; a lab boot gnomon cannot give is noted, not failed."""
        if lab_boot is None:
            lab_boot = self._lab_boot_after(run, before, returned)
            if lab_boot is None:
                return
        # journalctl -b takes the undashed id; /proc prints boot ids dashed.
        result = self.layer.remote(
            ["sudo", "-n", "journalctl", "-b", _boot_key(lab_boot), "-o", "json", "--no-pager"]
        )
        if result.returncode != 0:
            detail = result.stderr.strip() or "no output"
            run.say(
                f"journal: journalctl -b {lab_boot} failed (rc {result.returncode}): {detail}; "
                "journal.json not written"
            )
            return
        (run.directory / "journal.json").write_bytes(result.stdout.encode("utf-8"))

    def _lab_boot_after(self, run: RunLog, before: str, returned: str) -> str | None:
        """The boot list-boots lists right after `before`, or None when the lab boot left no journal."""
        result = self.layer.remote(["sudo", "-n", "journalctl", "--list-boots", "-o", "json", "--no-pager"])
        if result.returncode != 0:
            detail = result.stderr.strip() or "no output"
            run.say(
                f"journal: journalctl --list-boots failed (rc {result.returncode}): {detail}; "
                "journal.json not written"
            )
            return None
        try:
            boots = parse_boot_list(result.stdout)
        except LabError as err:
            run.say(f"journal: {err}; journal.json not written")
            return None
        keys = [_boot_key(boot) for boot in boots]
        if _boot_key(before) not in keys:
            run.say(f"journal: boot {before} from before the run is not listed; journal.json not written")
            return None
        following = keys.index(_boot_key(before)) + 1
        if following == len(boots) or keys[following] == _boot_key(returned):
            run.say("journal: lab boot left no journal; journal.json not written")
            return None
        return boots[following]

    def _abort(self, run: RunLog) -> None:
        run.say("aborting: removing the lab entry and clearing the one-shot")
        for argv in (
            ["sudo", "-n", "rm", "-f", ESP_ENTRY],
            ["sudo", "-n", "bootctl", "set-oneshot", ""],
        ):
            result = self.layer.remote(argv)
            if result.returncode != 0:
                detail = result.stderr.strip() or result.stdout.strip()
                run.say(
                    f"abort step failed: {shlex.join(argv)}: {detail}; "
                    f"{ESP_ENTRY} and the one-shot may need manual cleanup on gnomon"
                )


def main(
    argv: Sequence[str] | None = None,
    *,
    layer: Layer | None = None,
    clock: Clock | None = None,
) -> int:
    parser = argparse.ArgumentParser(
        description="Boot gnomon into lab experiments and verify it returns to its default."
    )
    parser.add_argument("experiments", nargs="+", metavar="EXPERIMENT", choices=sorted(EXPERIMENTS))
    parser.add_argument(
        "--nix-config",
        type=Path,
        default=Path.home() / "nix-config",
        help="nix-config checkout that exposes labSystems",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="halmasuit checkout whose HEAD is built into the lab images",
    )
    parser.add_argument(
        "--state-root",
        type=Path,
        default=Path.home() / ".local" / "state" / "halmasuit-lab" / "runs",
        help="directory for per-run logs",
    )
    parser.add_argument("--wol-iface", default="enp7s0", help="gnomon interface read for its WoL MAC")
    parser.add_argument(
        "--display-manager", default="display-manager.service", help="unit that must be active after return"
    )
    parser.add_argument(
        "--netconsole-port",
        type=int,
        default=NETCONSOLE_PORT,
        help="UDP port that receives gnomon's netconsole stream",
    )
    parser.add_argument(
        "--probe-user",
        default=DEFAULT_PROBE_USER,
        help="user whose pam_unix greetd failure must appear in the lab boot journal for the probe to pass",
    )
    options = parser.parse_args(argv)
    runner = Lab(
        layer or SystemLayer(),
        clock or SystemClock(),
        repo_root=options.repo_root,
        nix_config=options.nix_config,
        state_root=options.state_root,
        wol_iface=options.wol_iface,
        display_manager=options.display_manager,
        netconsole_port=options.netconsole_port,
        probe_user=options.probe_user,
        emit=print,
    )
    for name in options.experiments:
        try:
            runner.run_experiment(name)
        except LabError as err:
            print(f"lab-run: {name} failed: {err}; queue stopped", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
