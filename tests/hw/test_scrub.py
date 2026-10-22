#!/usr/bin/env python3
"""Selftest for tests/hw/scrub.py.

Each finding class is pinned on a known input, so a detector that misses a
class or flags a documentation value fails here. The committed-fixture test
scans the real tests/hw/fixtures/ tree and fails on any finding.
"""

import tempfile
import unittest
from pathlib import Path

import scrub

UNLISTED_ID = "0123456789abcdef0123456789abcdef"
USERNAME = "josh" + "symonds"
REVIEWED_IDS = (
    "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e",
    "b007b007b007b007b007b007b007b007",
    "7d4958e842da4a758f6c1cdc7b36dcc5",
    "39f53479d3a045ac8e11786248231fbf",
    "4a67b0820a4c41cfb6c7440b29bb8c4f",
)


def dashed(hex32: str) -> str:
    return f"{hex32[:8]}-{hex32[8:12]}-{hex32[12:16]}-{hex32[16:20]}-{hex32[20:]}"


def hits(data: bytes) -> list[tuple[str, str]]:
    return scrub.scan_bytes(data)


class MachineIdTest(unittest.TestCase):
    def test_unlisted_id_is_flagged(self):
        self.assertEqual(
            hits(f"machine-id={UNLISTED_ID}\n".encode()),
            [("machine-id", UNLISTED_ID)],
        )

    def test_unlisted_dashed_uuid_is_flagged(self):
        self.assertEqual(
            hits(dashed(UNLISTED_ID).encode()),
            [("machine-id", dashed(UNLISTED_ID))],
        )

    def test_unlisted_uppercase_id_is_flagged(self):
        upper = UNLISTED_ID.upper()
        self.assertEqual(hits(upper.encode()), [("machine-id", upper)])

    def test_reviewed_ids_pass_in_every_form(self):
        for ident in REVIEWED_IDS:
            for form in (ident, ident.upper(), dashed(ident), dashed(ident).upper()):
                with self.subTest(form=form):
                    self.assertEqual(hits(form.encode()), [])

    def test_id_inside_longer_hex_run_is_not_flagged(self):
        self.assertEqual(hits(("0" + UNLISTED_ID).encode()), [])
        self.assertEqual(hits((UNLISTED_ID + "0").encode()), [])


class MacTest(unittest.TestCase):
    def test_colon_separated_mac_is_flagged(self):
        self.assertEqual(
            hits(b"ether 02:00:00:0a:0b:0c"),
            [("mac", "02:00:00:0a:0b:0c")],
        )

    def test_dash_separated_uppercase_mac_is_flagged(self):
        self.assertEqual(
            hits(b"02-00-00-0A-0B-0C"),
            [("mac", "02-00-00-0A-0B-0C")],
        )

    def test_five_octets_are_not_a_mac(self):
        self.assertEqual(hits(b"02:00:00:0a:0b"), [])


class Ipv4Test(unittest.TestCase):
    def test_addresses_outside_documentation_ranges_are_flagged(self):
        for addr in ("10.0.0.5", "8.8.8.8", "192.0.3.1", "192.168.1.20"):
            with self.subTest(addr=addr):
                self.assertEqual(hits(f"addr {addr} ".encode()), [("ipv4", addr)])

    def test_documentation_ranges_pass(self):
        for addr in ("192.0.2.1", "198.51.100.254", "203.0.113.7"):
            with self.subTest(addr=addr):
                self.assertEqual(hits(f"addr {addr} ".encode()), [])

    def test_out_of_range_octet_is_not_an_address(self):
        self.assertEqual(hits(b"256.1.1.1"), [])

    def test_address_inside_longer_dotted_number_is_not_flagged(self):
        self.assertEqual(hits(b"version 1.10.20.30.40"), [])

    def test_address_after_a_non_numeric_dot_is_flagged(self):
        self.assertEqual(hits(b"connecting...10.0.0.5"), [("ipv4", "10.0.0.5")])
        self.assertEqual(hits(b"lease.192.168.1.20 "), [("ipv4", "192.168.1.20")])


class Ipv6Test(unittest.TestCase):
    def test_addresses_outside_documentation_range_are_flagged(self):
        for addr in ("fe80::1", "2001:db9::1", "2606:4700:4700::1111"):
            with self.subTest(addr=addr):
                self.assertEqual(hits(f"addr {addr} ".encode()), [("ipv6", addr)])

    def test_documentation_range_passes(self):
        for addr in ("2001:db8::1", "2001:DB8:0:0:0:0:0:1"):
            with self.subTest(addr=addr):
                self.assertEqual(hits(f"addr {addr} ".encode()), [])

    def test_clock_time_is_not_an_address(self):
        self.assertEqual(hits(b"12:34:56"), [])

    def test_path_separator_between_identifiers_is_not_an_address(self):
        for text in ("halmasuit::event", "smithay::backend::drm"):
            with self.subTest(text=text):
                self.assertEqual(hits(text.encode()), [])

    def test_path_separator_needs_identifier_on_both_sides(self):
        for text, expected in (
            ("::e", [("ipv6", "::e")]),
            ("halmasuit::", [("ipv6", "::")]),
            ("xa:b::cy", [("ipv6", "a:b::c")]),
            ("é::eé", [("ipv6", "::e")]),
        ):
            with self.subTest(text=text):
                self.assertEqual(hits(text.encode()), expected)

    def test_candidate_glued_to_a_word_is_an_address(self):
        for text, addr in (
            ("ipv6:2600:1700:abcd::1", "6:2600:1700:abcd::1"),
            ("eth0:fe80::1", "0:fe80::1"),
            ("tcp6:2600:1700::5", "6:2600:1700::5"),
            ("src:2600:1700::5", "c:2600:1700::5"),
            ("2600:1700::5x", "2600:1700::5"),
            ("from2600:1700::5", "2600:1700::5"),
            ("v2606:4700::1111", "2606:4700::1111"),
            ("fe80::1g", "fe80::1"),
        ):
            with self.subTest(text=text):
                self.assertEqual(hits(text.encode()), [("ipv6", addr)])

    def test_run_ending_in_a_colon_is_not_an_address(self):
        self.assertEqual(hits(b"fe80::1:g"), [])

    def test_tracing_target_in_journal_row_is_not_flagged(self):
        row = (
            b'{"timestamp":"2026-10-22T12:00:00Z","level":"INFO",'
            b'"fields":{"json":"{}"},"target":"halmasuit::event"}'
        )
        self.assertEqual(hits(row), [])

    def test_address_beside_non_ascii_bytes_is_still_flagged(self):
        for data in (
            b"fe80::1\xff",
            b"\xff\xff\xffinet6 fe80::1\xff\xff\xff",
            "\u2018fe80::1\u2019".encode(),
            "fe80::1\u4e00".encode("utf-16-le"),
        ):
            with self.subTest(data=data):
                self.assertIn(("ipv6", "fe80::1"), hits(data))

    def test_standalone_addresses_in_context_are_still_flagged(self):
        for text, addr in (
            ("fe80::1", "fe80::1"),
            ("inet6 fe80::1/64", "fe80::1"),
            ("[2600:1700::5]:443", "2600:1700::5"),
            ("src=2600:1700::5", "2600:1700::5"),
            ("addr=2600:1700::5", "2600:1700::5"),
            ('"2600:1700::5"', "2600:1700::5"),
        ):
            with self.subTest(text=text):
                self.assertEqual(hits(text.encode()), [("ipv6", addr)])


class UsernameTest(unittest.TestCase):
    def test_username_in_any_case_is_flagged(self):
        alternating = "".join(c.upper() if i % 2 else c for i, c in enumerate(USERNAME))
        for variant in (USERNAME, USERNAME.upper(), USERNAME.capitalize(), alternating):
            with self.subTest(variant=variant):
                self.assertEqual(
                    hits(f"/home/{variant}".encode()),
                    [("username", variant)],
                )


class EncodingTest(unittest.TestCase):
    def test_utf16le_machine_id_is_flagged(self):
        self.assertEqual(
            hits(UNLISTED_ID.encode("utf-16-le")),
            [("machine-id", UNLISTED_ID)],
        )

    def test_utf16le_at_odd_offset_is_flagged(self):
        self.assertEqual(
            hits(b"\x01" + UNLISTED_ID.encode("utf-16-le")),
            [("machine-id", UNLISTED_ID)],
        )

    def test_utf16le_username_is_flagged(self):
        self.assertEqual(
            hits(USERNAME.encode("utf-16-le")),
            [("username", USERNAME)],
        )


class ScanTreeTest(unittest.TestCase):
    def test_absent_root_has_no_findings(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(scrub.scan_tree(Path(tmp) / "missing"), [])

    def test_findings_carry_root_relative_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "clean.txt").write_text("addr 192.0.2.1\n")
            (root / "bin").mkdir()
            (root / "bin" / "efivar.bin").write_bytes(UNLISTED_ID.encode("utf-16-le"))
            self.assertEqual(
                scrub.scan_tree(root),
                [scrub.Finding("bin/efivar.bin", "machine-id", UNLISTED_ID)],
            )

    def test_committed_fixtures_have_no_findings(self):
        self.assertEqual(scrub.scan_tree(scrub.FIXTURE_ROOT), [])


if __name__ == "__main__":
    unittest.main()
