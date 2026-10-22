#!/usr/bin/env python3
"""Vermissian-side lab runner: boots gnomon into lab UKIs and back.

Per experiment: realise the lab toplevel and UKI from nix-config's `labSystems`
output in gnomon's store (evaluated here, built by gnomon), list the UKI image
on gnomon, sign it with the sbctl db key into a staging path, check the ESP has
room, record the boot entries, default entry, and system generations, install
the UKI as a one-shot ESP entry, reboot, and verify gnomon returns to the
pre-run default with the pre-run record intact.
The lab entry is removed on the way out.

Every remote, local, clock, and magic-packet effect goes through the `Layer`
and `Clock` objects handed to `Lab`, so test_lab.py runs the whole sequence
against a simulated gnomon.

A run refuses before it installs anything, and again before each reboot it
issues, while a graphical user session (Class=user, Type wayland, x11, or mir)
is logged in. Any failure after the install removes the lab entry and clears
the one-shot before the runner exits.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import socket
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple, Protocol, TypeVar

T = TypeVar("T")
REPO_ROOT = Path(__file__).resolve().parents[2]
GNOMON = "gnomon"
LAB_ENTRY = "halmasuit-lab.efi"
ESP_ENTRY = f"/boot/EFI/Linux/{LAB_ENTRY}"
STAGED_DIR = "/var/lib/halmasuit-lab"
STAGED = f"{STAGED_DIR}/{LAB_ENTRY}"
BOOT_ID_FILE = "/proc/sys/kernel/random/boot_id"
SELECTED_VAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
ESP_MARGIN_BYTES = 200 * 1024 * 1024
GRAPHICAL_TYPES = frozenset({"wayland", "x11", "mir"})
MAC_PATTERN = re.compile(r"[0-9a-f]{2}(:[0-9a-f]{2}){5}")
POLL_SECONDS = 5
BOOT_DEADLINE_SECONDS = 900
SHUTDOWN_DEADLINE_SECONDS = 600
DISPLAY_DEADLINE_SECONDS = 300
REMOTE_TIMEOUT_SECONDS = 120
MAGIC_PORT = 9


class LabError(Exception):
    """A lab step failed or refused; the queue stops at the first one."""


class Result(NamedTuple):
    returncode: int
    stdout: str
    stderr: str


class Layer(Protocol):
    """Every effect the runner has on the outside world."""

    def local(self, argv: Sequence[str]) -> Result: ...

    def remote(self, argv: Sequence[str]) -> Result: ...

    def wake(self, mac: str) -> None: ...


class Clock(Protocol):
    def monotonic(self) -> float: ...

    def sleep(self, seconds: float) -> None: ...

    def stamp(self) -> str: ...


class SystemLayer:
    """The real effects: local processes, ssh to the gnomon alias, a UDP broadcast."""

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


EXPERIMENTS: dict[str, Plan] = {
    "baseline": Plan(lab_boot_checked=True, poweroff=False),
    "fault-initrd": Plan(lab_boot_checked=False, poweroff=False),
    "fault-hang": Plan(lab_boot_checked=False, poweroff=False),
    "fault-panic": Plan(lab_boot_checked=False, poweroff=False),
    "fault-poweroff": Plan(lab_boot_checked=False, poweroff=True),
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


def parse_generations(profiles: str) -> tuple[int, ...]:
    numbers = []
    for name in profiles.split():
        match = re.fullmatch(r"system-(\d+)-link", name)
        if match:
            numbers.append(int(match.group(1)))
    return tuple(sorted(numbers))


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
        emit: Callable[[str], None],
    ) -> None:
        self.layer = layer
        self.clock = clock
        self.repo_root = repo_root
        self.nix_config = nix_config
        self.state_root = state_root
        self.wol_iface = wol_iface
        self.display_manager = display_manager
        self.emit = emit

    def run_experiment(self, name: str) -> None:
        plan = EXPERIMENTS[name]
        run = RunLog(self.state_root, self.clock.stamp(), name, self.emit)
        installed = False
        try:
            head = self._head_rev()
            run.say(f"experiment {name} from halmasuit {head}")
            self._build(name, "toplevel", head)
            uki = self._build(name, "uki", head)
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
            if plan.lab_boot_checked:
                booted_from = self._await_boot("lab entry to boot", boot_before, LAB_ENTRY)
                run.say(f"lab entry booted as {booted_from}")
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
            self._await_display_manager()
            self._remove_entry()
            _, post = self._record()
            _compare(pre, post)
            run.say("PASS")
        except BaseException as err:
            run.say(f"FAIL: {err}")
            if installed:
                self._abort(run)
            raise

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
    options = parser.parse_args(argv)
    runner = Lab(
        layer or SystemLayer(),
        clock or SystemClock(),
        repo_root=options.repo_root,
        nix_config=options.nix_config,
        state_root=options.state_root,
        wol_iface=options.wol_iface,
        display_manager=options.display_manager,
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
