#!/usr/bin/env python3
"""Selftest for tests/hw/pam_stack.py.

Every PAM file is supplied through an in-memory reader, so the resolver runs
without touching /etc/pam.d. Each test names the stack shape it exercises;
unreadable or cyclic stacks must raise PamStackError rather than resolve to a
lockout-free result.
"""

import unittest
from collections.abc import Mapping

import pam_stack
from pam_stack import PamStackError


def reader(files: Mapping[str, str]):
    return lambda name: files.get(name)


NIX_FAILLOCK = "/nix/store/aaaa-linux-pam-1.6.1/lib/security/pam_faillock.so"


class ResolveTests(unittest.TestCase):
    def test_direct_module_is_listed(self):
        stack = pam_stack.resolve("greetd", reader({"greetd": "auth required pam_unix.so\n"}))
        self.assertEqual(stack.files, ("greetd",))
        self.assertEqual(stack.modules, ("pam_unix",))
        self.assertEqual(stack.lockout, ())

    def test_lockout_free_stack_reports_no_lockout(self):
        files = {"greetd": "auth required pam_unix.so\nsession optional pam_systemd.so\n"}
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_unix", "pam_systemd"))
        self.assertEqual(stack.lockout, ())

    def test_each_lockout_module_is_reported(self):
        for name in ("pam_faillock", "pam_tally", "pam_tally2"):
            with self.subTest(module=name):
                files = {"greetd": f"auth required {name}.so deny=3\n"}
                stack = pam_stack.resolve("greetd", reader(files))
                self.assertEqual(stack.lockout, (name,))

    def test_include_is_followed(self):
        files = {
            "greetd": "auth include common-auth\n",
            "common-auth": "auth required pam_faillock.so preauth\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.files, ("greetd", "common-auth"))
        self.assertEqual(stack.lockout, ("pam_faillock",))

    def test_substack_is_followed_with_nixos_store_path(self):
        files = {
            "greetd": "auth substack login\n",
            "login": f"auth required {NIX_FAILLOCK} preauth\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.files, ("greetd", "login"))
        self.assertEqual(stack.lockout, ("pam_faillock",))

    def test_at_include_is_followed(self):
        files = {
            "greetd": "@include common-account\n",
            "common-account": "account required pam_tally2.so\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.files, ("greetd", "common-account"))
        self.assertEqual(stack.lockout, ("pam_tally2",))

    def test_lockout_behind_deep_include_chain_is_reported(self):
        files = {
            "greetd": "auth include a\n",
            "a": "auth substack b\n",
            "b": "@include c\n",
            "c": "auth required pam_tally.so\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.files, ("greetd", "a", "b", "c"))
        self.assertEqual(stack.lockout, ("pam_tally",))

    def test_modules_keep_stack_order_across_includes(self):
        files = {
            "greetd": "auth required pam_env.so\nauth include common\nsession optional pam_systemd.so\n",
            "common": "auth required pam_unix.so\nauth required pam_faillock.so\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_env", "pam_unix", "pam_faillock", "pam_systemd"))
        self.assertEqual(stack.lockout, ("pam_faillock",))

    def test_dash_type_still_counts_as_present(self):
        files = {"greetd": "-auth optional pam_faillock.so\n"}
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.lockout, ("pam_faillock",))

    def test_bracketed_control_with_spaces_is_parsed(self):
        files = {
            "greetd": "auth [success=1 default=bad] pam_tally2.so deny=3 unlock_time=60\n"
            "auth required pam_unix.so nullok\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_tally2", "pam_unix"))
        self.assertEqual(stack.lockout, ("pam_tally2",))

    def test_absolute_store_path_yields_basename_module(self):
        files = {"greetd": f"session required {NIX_FAILLOCK}\n"}
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_faillock",))

    def test_backslash_continuation_joins_lines(self):
        files = {"greetd": "auth required \\\n    pam_faillock.so preauth\n"}
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_faillock",))
        self.assertEqual(stack.lockout, ("pam_faillock",))

    def test_blank_and_comment_lines_are_skipped(self):
        files = {"greetd": "\n# auth required pam_faillock.so\n   \nauth required pam_unix.so\n"}
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.modules, ("pam_unix",))
        self.assertEqual(stack.lockout, ())

    def test_shared_include_in_diamond_is_not_a_cycle(self):
        files = {
            "greetd": "auth include common\nauth substack login\n",
            "login": "auth include common\n",
            "common": "auth required pam_tally2.so\n",
        }
        stack = pam_stack.resolve("greetd", reader(files))
        self.assertEqual(stack.files, ("greetd", "common", "login"))
        self.assertIn("pam_tally2", stack.lockout)


class RefusalTests(unittest.TestCase):
    def assertRefuses(self, service, files):
        with self.assertRaises(PamStackError):
            pam_stack.resolve(service, reader(files))

    def test_missing_service_file_raises(self):
        self.assertRefuses("greetd", {})

    def test_missing_included_file_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth include gone\n"})

    def test_missing_at_include_file_raises(self):
        self.assertRefuses("greetd", {"greetd": "@include gone\n"})

    def test_include_cycle_raises(self):
        self.assertRefuses("greetd", {
            "greetd": "auth include a\n",
            "a": "auth substack greetd\n",
        })

    def test_self_include_raises(self):
        self.assertRefuses("greetd", {"greetd": "@include greetd\n"})

    def test_line_with_only_type_and_control_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth required\n"})

    def test_line_with_only_type_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth\n"})

    def test_unknown_type_raises(self):
        self.assertRefuses("greetd", {"greetd": "bogus required pam_unix.so\n"})

    def test_unclosed_bracket_control_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth [success=1 pam_tally2.so\n"})

    def test_include_without_service_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth include\n"})

    def test_at_include_without_service_raises(self):
        self.assertRefuses("greetd", {"greetd": "@include\n"})

    def test_service_name_with_path_separator_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth include ../shadow\n"})

    def test_service_name_with_leading_dot_raises(self):
        self.assertRefuses("greetd", {"greetd": "auth include ..\n"})


class ProducedValueTests(unittest.TestCase):
    def test_result_is_frozen(self):
        stack = pam_stack.resolve("greetd", reader({"greetd": "auth required pam_unix.so\n"}))
        with self.assertRaises(AttributeError):
            stack.modules = ()

    def test_lockout_modules_frozenset_is_exact(self):
        self.assertEqual(pam_stack.LOCKOUT_MODULES, frozenset({"pam_faillock", "pam_tally", "pam_tally2"}))


if __name__ == "__main__":
    unittest.main()
