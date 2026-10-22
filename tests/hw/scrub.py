#!/usr/bin/env python3
"""Fixture scrub check for tests/hw/fixtures/.

Scans every file under the fixture root and reports machine-id-shaped ids not
on ALLOWED_IDS, MAC addresses, IPv4 and IPv6 addresses outside the
documentation ranges, and occurrences of the username. Each file is decoded as
latin-1 and as UTF-16LE at byte offsets 0 and 1, so text and binary recordings
(FPDT, boot_params, efivars, BMP) are covered.
"""

from __future__ import annotations

import ipaddress
import re
import string
import sys
from pathlib import Path
from typing import NamedTuple

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURE_ROOT = REPO_ROOT / "tests" / "hw" / "fixtures"

ALLOWED_IDS = frozenset(
    {
        "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e",  # synthetic machine id
        "b007b007b007b007b007b007b007b007",  # synthetic boot id
        "7d4958e842da4a758f6c1cdc7b36dcc5",  # systemd MESSAGE_ID, unit starting
        "39f53479d3a045ac8e11786248231fbf",  # systemd MESSAGE_ID, unit started
        "4a67b0820a4c41cfb6c7440b29bb8c4f",  # systemd-boot vendor GUID
    }
)

HEX32 = re.compile(r"(?<![0-9a-fA-F])([0-9a-fA-F]{32})(?![0-9a-fA-F])")
DASHED_UUID = re.compile(
    r"(?<![0-9a-fA-F])([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}"
    r"-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})(?![0-9a-fA-F])"
)
MAC = re.compile(r"(?<![0-9a-fA-F])[0-9a-fA-F]{2}(?:[:-][0-9a-fA-F]{2}){5}(?![0-9a-fA-F])")
IPV4_CANDIDATE = re.compile(r"(?<!\d)(?<!\d\.)\d{1,3}(?:\.\d{1,3}){3}(?!\.?\d)")
# A candidate is a maximal run of hex digits and colons. A run is a language path
# separator, not an address, when an ASCII identifier character sits directly
# before and directly after it and its only colons are one `::` (`halmasuit::event`,
# `smithay::backend::drm`). Every other run is parsed as an address.
IPV6_CANDIDATE = re.compile(r"[0-9a-fA-F:]+")
ASCII_IDENT = frozenset(string.ascii_letters + string.digits + "_")
DOC_IPV4 = tuple(
    ipaddress.ip_network(net)
    for net in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24")
)
DOC_IPV6 = ipaddress.ip_network("2001:db8::/32")
USERNAME = re.compile("josh" + "symonds", re.IGNORECASE)


class Finding(NamedTuple):
    path: str
    kind: str
    value: str


def _texts(data: bytes) -> list[str]:
    return [
        data.decode("latin-1"),
        data.decode("utf-16-le", errors="ignore"),
        data[1:].decode("utf-16-le", errors="ignore"),
    ]


def _is_path_separator(text: str, match: re.Match[str]) -> bool:
    token = match.group()
    if token.count(":") != 2 or "::" not in token:
        return False
    before = text[match.start() - 1] if match.start() > 0 else ""
    after = text[match.end()] if match.end() < len(text) else ""
    return before in ASCII_IDENT and after in ASCII_IDENT


def _scan_text(text: str) -> list[tuple[str, str]]:
    hits: list[tuple[str, str]] = []
    for match in HEX32.finditer(text):
        if match.group(1).lower() not in ALLOWED_IDS:
            hits.append(("machine-id", match.group(1)))
    for match in DASHED_UUID.finditer(text):
        if match.group(1).replace("-", "").lower() not in ALLOWED_IDS:
            hits.append(("machine-id", match.group(1)))
    for match in MAC.finditer(text):
        hits.append(("mac", match.group()))
    for match in IPV4_CANDIDATE.finditer(text):
        try:
            addr = ipaddress.IPv4Address(match.group())
        except ValueError:
            continue
        if not any(addr in net for net in DOC_IPV4):
            hits.append(("ipv4", match.group()))
    for match in IPV6_CANDIDATE.finditer(text):
        token = match.group()
        if token.count(":") < 2 or _is_path_separator(text, match):
            continue
        try:
            addr6 = ipaddress.IPv6Address(token)
        except ValueError:
            continue
        if addr6 not in DOC_IPV6:
            hits.append(("ipv6", token))
    for match in USERNAME.finditer(text):
        hits.append(("username", match.group()))
    return hits


def scan_bytes(data: bytes) -> list[tuple[str, str]]:
    hits: list[tuple[str, str]] = []
    for text in _texts(data):
        hits.extend(_scan_text(text))
    return hits


def scan_tree(root: Path = FIXTURE_ROOT) -> list[Finding]:
    if not root.is_dir():
        return []
    findings: list[Finding] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        rel = path.relative_to(root).as_posix()
        for kind, value in scan_bytes(path.read_bytes()):
            findings.append(Finding(rel, kind, value))
    return findings


def main() -> int:
    findings = scan_tree()
    for finding in findings:
        print(f"{finding.path}: {finding.kind} {finding.value}")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
