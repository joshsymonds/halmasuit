#!/usr/bin/env python3
"""Resolve a PAM service's full stack and name its lockout modules.

The stack is read through a caller-supplied `read(name)` callable that returns
the text of /etc/pam.d/<name>, or None when the file does not exist. Lines follow
pam.conf(5): `type control module-path [args]`, with blank and `#` lines skipped,
a trailing backslash continuing a line, a leading `-` on the type, a bracketed
control, `include` and `substack` naming a service to resolve recursively, and
`@include <service>` including a whole file. Any file that is missing, any
malformed line, and any include cycle raises PamStackError, so an unresolved
stack never reads as lockout-free.

Besides the module list, the resolved stack reports `nullok_auth`: every `auth`
line whose module is `pam_unix` and whose arguments (up to any `#` comment)
include `nullok`, as `"<service file>: <logical line>"`, listed once in first-seen
order. Such a line accepts a blank password, so the caller refuses on it.
"""

from __future__ import annotations

import re
from collections.abc import Callable
from dataclasses import dataclass

LOCKOUT_MODULES = frozenset({"pam_faillock", "pam_tally", "pam_tally2"})
PAM_TYPES = frozenset({"auth", "account", "password", "session"})
INCLUDE_CONTROLS = frozenset({"include", "substack"})
SERVICE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


class PamStackError(Exception):
    """A PAM stack could not be fully resolved; the caller refuses the probe."""


@dataclass(frozen=True)
class PamStack:
    files: tuple[str, ...]
    modules: tuple[str, ...]
    nullok_auth: tuple[str, ...]

    @property
    def lockout(self) -> tuple[str, ...]:
        return tuple(m for m in self.modules if m in LOCKOUT_MODULES)


def resolve(service: str, read: Callable[[str], str | None]) -> PamStack:
    files: list[str] = []
    modules: list[str] = []
    nullok: list[str] = []
    _walk(service, read, (), files, modules, nullok)
    return PamStack(files=tuple(files), modules=tuple(modules), nullok_auth=tuple(nullok))


def _walk(
    service: str,
    read: Callable[[str], str | None],
    ancestors: tuple[str, ...],
    files: list[str],
    modules: list[str],
    nullok: list[str],
) -> None:
    if not SERVICE_NAME.fullmatch(service):
        raise PamStackError(f"invalid PAM service name {service!r}")
    if service in ancestors:
        raise PamStackError(f"PAM include cycle through {service!r}")
    text = read(service)
    if text is None:
        raise PamStackError(f"PAM service file {service!r} does not exist")
    if service not in files:
        files.append(service)
    chain = (*ancestors, service)
    for line in _logical_lines(text):
        fields = line.split()
        if not fields or fields[0].startswith("#"):
            continue
        if fields[0] == "@include":
            if len(fields) != 2:
                raise PamStackError(f"{service!r}: malformed line {line!r}")
            _walk(fields[1], read, chain, files, modules, nullok)
            continue
        kind, control, args = _split_entry(service, line)
        if kind.removeprefix("-") not in PAM_TYPES or not args:
            raise PamStackError(f"{service!r}: malformed line {line!r}")
        if control in INCLUDE_CONTROLS:
            _walk(args[0], read, chain, files, modules, nullok)
        else:
            module = _module_name(args[0])
            modules.append(module)
            if kind.removeprefix("-") == "auth" and module == "pam_unix" and _has_nullok(args[1:]):
                entry = f"{service}: {line.strip()}"
                if entry not in nullok:
                    nullok.append(entry)


def _logical_lines(text: str) -> list[str]:
    lines: list[str] = []
    pending = ""
    for raw in text.splitlines():
        stripped = raw.rstrip()
        if stripped.endswith("\\"):
            pending += stripped[:-1] + " "
            continue
        lines.append(pending + raw)
        pending = ""
    if pending:
        lines.append(pending)
    return lines


def _split_entry(service: str, line: str) -> tuple[str, str, list[str]]:
    parts = line.split(None, 1)
    if len(parts) != 2:
        raise PamStackError(f"{service!r}: malformed line {line!r}")
    kind, rest = parts[0], parts[1].lstrip()
    if rest.startswith("["):
        end = rest.find("]")
        if end == -1:
            raise PamStackError(f"{service!r}: unclosed control in {line!r}")
        control, tail = rest[: end + 1], rest[end + 1 :]
    else:
        control_parts = rest.split(None, 1)
        control = control_parts[0]
        tail = control_parts[1] if len(control_parts) == 2 else ""
    return kind, control, tail.split()


def _module_name(module_path: str) -> str:
    base = module_path.rsplit("/", 1)[-1]
    return base.removesuffix(".so")


def _has_nullok(arguments: list[str]) -> bool:
    comment = next(
        (i for i, token in enumerate(arguments) if token.startswith("#")),
        len(arguments),
    )
    return "nullok" in arguments[:comment]
