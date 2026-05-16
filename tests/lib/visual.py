"""Visual-test helpers for halmasuit NixOS tests.

Importable from a `testScript` via::

    import sys
    sys.path.insert(0, "${./lib}")
    import visual

The module wraps three concerns:

  * `capture(machine, name)` — calls halmasuit's `Snapshot()` D-Bus
    method (a CPU readback of the real composited frame, NOT a QMP
    screenshot of QEMU's display) and copies the resulting PNG to the
    driver's output directory (`${out}` for store-built tests;
    `/tmp/...` for interactive runs).

  * `ssimulacra2_compare(expected, actual)` — shells out to the
    `ssimulacra2_rs` CLI and returns the perceptual similarity score
    as a float. Higher is closer; identical is 100.0; very different
    can be negative. Threshold ≥ 90.0 is the epic-locked default
    (JND-aligned "imperceptible"; corresponds to the architectural
    intent of "tight, allows minor swrast jitter").

  * `assert_matches_golden(machine, name)` — captures a screenshot,
    compares against `tests/goldens/{name}.png`, and either passes or
    raises with a useful diff-path message. The golden's location is
    supplied to the helper via the `GOLDENS_DIR` environment variable
    so the helper doesn't have to guess.

Two environment variables tune behavior:

  * `HALMASUIT_GOLDEN_REGEN=1` — `assert_matches_golden` copies the
    fresh capture to the golden path instead of comparing. The path
    printed on stderr is host-side; the developer commits it after
    visual inspection. Never set in CI.

  * `GOLDENS_DIR` — host-side path to the goldens directory, supplied
    by the NixOS test wrapper. Required for `assert_matches_golden`.

No threshold below 90.0 globally (epic anti-pattern). Per-test override
via the `threshold` kwarg is permitted only with a
`# SSIMULACRA2_OVERRIDE: <score> reason: ...` comment in the calling
test (enforced by humans, not this module).

Score convention: ssimulacra2_rs prints `Score: <float>` on a single
line. We parse the float after `Score:`.
"""

import json
import math
import os
import shutil
import subprocess
import sys
from pathlib import Path


DEFAULT_THRESHOLD = 90.0


# Guest-side directory the Snapshot() PNG is written into. The visual
# test machine config must create this writable by halmasuit's
# post-privilege-drop uid and add it to the unit's ReadWritePaths
# (halmasuit runs under ProtectSystem=strict). Kept in sync with the
# `tmpfiles`/`ReadWritePaths` wiring in tests/visual-halmasuit-*.nix.
GUEST_SNAPSHOT_DIR = "/run/hsnap"


def capture(machine, name: str) -> Path:
    """Capture the real composited frame via halmasuit's `Snapshot()`
    D-Bus method and return the host-side PNG path.

    This is NOT a QMP screenshot of QEMU's display. We call
    `org.halmasuit.Debug.Introspect.Snapshot` (present only in the
    `frame_audit`/`halmasuit-debug` build), which does a CPU readback
    of the exact frame halmasuit composited and PNG-encodes it. The
    test must already have waited for the bus name to be owned before
    calling this (a frame must have been composited, too — Snapshot
    errors loudly otherwise).

    The PNG is written guest-side into ``GUEST_SNAPSHOT_DIR`` then
    copied to the driver's output directory.
    """
    guest_path = f"{GUEST_SNAPSHOT_DIR}/{name}.png"
    # `busctl call ... Snapshot s <path>` — the `s` signature is the
    # single string arg. machine.succeed raises with the guest stderr
    # if the method returns a D-Bus error (no frame yet, unwritable
    # path, name not owned).
    machine.succeed(
        "busctl --system call org.halmasuit "
        "/org/halmasuit/Debug/Introspect "
        f"org.halmasuit.Debug.Introspect Snapshot s {guest_path!r}"
    )
    # Pull the guest file into the test driver's output dir.
    machine.copy_from_vm(guest_path)
    out_dir = os.environ.get("out") or os.environ.get("TMPDIR") or "/tmp"
    for d in (out_dir, "/tmp", "."):
        p = Path(d) / f"{name}.png"
        if p.exists():
            return p
    raise FileNotFoundError(
        f"Snapshot {name!r} not found after copy_from_vm({guest_path}); "
        f"searched out={out_dir}, /tmp, cwd. Listing {out_dir}:\n"
        + _safe_listdir(out_dir)
    )


def _safe_listdir(path: str) -> str:
    try:
        return "\n".join(sorted(os.listdir(path)))
    except OSError as exc:
        return f"<listdir failed: {exc}>"


def ssimulacra2_compare(expected: Path, actual: Path) -> float:
    """Run `ssimulacra2_rs image expected actual` and return the
    perceptual similarity score.

    Higher = more similar. 100.0 = identical. ≥ 90 = imperceptible.
    Negative scores are possible for very-different images.

    The CLI prints `Score: <float>` on a single line. We extract the
    first float token following the literal `Score:`.
    """
    expected = Path(expected)
    actual = Path(actual)
    if not expected.exists():
        raise FileNotFoundError(f"expected golden not found: {expected}")
    if not actual.exists():
        raise FileNotFoundError(f"actual capture not found: {actual}")
    result = subprocess.run(
        ["ssimulacra2_rs", "image", str(expected), str(actual)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"ssimulacra2_rs failed (exit {result.returncode}):\n"
            f"  expected: {expected}\n"
            f"  actual:   {actual}\n"
            f"  stdout:   {result.stdout!r}\n"
            f"  stderr:   {result.stderr!r}"
        )
    score_line = None
    for line in result.stdout.splitlines():
        if line.startswith("Score:"):
            score_line = line
            break
    if score_line is None:
        raise RuntimeError(
            f"ssimulacra2_rs produced no 'Score:' line for {expected} vs {actual}\n"
            f"  full stdout: {result.stdout!r}"
        )
    token = score_line.split(":", 1)[1].strip().split()[0]
    try:
        score = float(token)
    except ValueError as exc:
        raise RuntimeError(
            f"could not parse ssimulacra2_rs score from {score_line!r}: {exc}"
        ) from exc
    # NaN guard: `float('nan') < threshold` is False in Python, so a
    # garbled CLI output that yields NaN would silently pass the
    # `score < threshold` assertion in assert_matches_golden. Refuse
    # non-finite scores explicitly so the failure mode is loud.
    if not math.isfinite(score):
        raise RuntimeError(
            f"ssimulacra2_rs returned non-finite score {score!r} "
            f"(parsed from {score_line!r})"
        )
    return score


def assert_matches_golden(
    machine,
    name: str,
    *,
    threshold: float = DEFAULT_THRESHOLD,
) -> None:
    """Capture a screenshot named `name`, compare to
    `${GOLDENS_DIR}/{name}.png`, raise if `score < threshold`.

    If `HALMASUIT_GOLDEN_REGEN=1` is set in the environment, the
    captured screenshot is copied to the golden path instead and the
    function returns without asserting. The developer is expected to
    visually inspect the new golden before committing it.
    """
    actual = capture(machine, name)
    goldens_dir = os.environ.get("GOLDENS_DIR")
    if not goldens_dir:
        raise RuntimeError(
            "GOLDENS_DIR env var not set; the test wrapper must export "
            "the host-side path to tests/goldens so visual.py can find "
            "the checked-in PNGs."
        )
    golden = Path(goldens_dir) / f"{name}.png"

    if os.environ.get("HALMASUIT_GOLDEN_REGEN") == "1":
        golden.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(actual, golden)
        print(
            f"REGEN: wrote {golden} from {actual}. "
            "Inspect manually before committing.",
            file=sys.stderr,
        )
        return

    if not golden.exists():
        raise FileNotFoundError(
            f"golden {golden} not found. Regenerate with "
            f"HALMASUIT_GOLDEN_REGEN=1, inspect the PNG manually, "
            f"then commit it.\n"
            f"Fresh capture is at: {actual}"
        )

    score = ssimulacra2_compare(golden, actual)
    print(json.dumps({"golden": name, "score": score, "threshold": threshold}))
    if score < threshold:
        raise AssertionError(
            f"golden {name!r} mismatch: score {score:.4f} < threshold {threshold:.4f}\n"
            f"  expected: {golden}\n"
            f"  actual:   {actual}\n"
            f"To accept: HALMASUIT_GOLDEN_REGEN=1 just update-goldens {name}\n"
            f"To inspect: compare the two PNGs visually before regenerating."
        )


def introspect_events(machine) -> list:
    """Return halmasuit's introspection Event stream, in journal
    (chronological) order, as a list of dicts.

    `halmasuit-introspect::emit` serializes each `Event` to JSON and
    logs it via tracing-subscriber's JSON formatter; journald carries
    that line. `journalctl -o cat` yields the raw tracing line
    (`{"timestamp":...,"fields":{"json":"<Event JSON string>"},...}`);
    the Event itself is the JSON-encoded `fields.json` string.
    """
    raw = machine.succeed("journalctl -u halmasuit -o cat --no-pager")
    events = []
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            outer = json.loads(line)
        except ValueError:
            continue
        inner = outer.get("fields", {}).get("json")
        if not inner:
            continue
        try:
            ev = json.loads(inner)
        except ValueError:
            continue
        if isinstance(ev, dict) and "event" in ev:
            events.append(ev)
    return events


def assert_frame_continuity(machine) -> None:
    """The no-flash invariant (Epic #1 req 11), asserted over the
    full `FrameRendered` stream — NOT sampled screenshots.

    - From the first `client_first_frame{role:background}` onward,
      every `frame_rendered` must have ``backdrop_coverage > 0.95``.
    - From the first `client_first_frame` of ANY role onward, no
      `frame_rendered` may have ``mean_luminance < 0.01`` (no black
      frame once a client has committed; pre-commit clear frames are
      exempt).

    Fails loudly, naming the offending event(s) and their index.
    """
    events = introspect_events(machine)

    first_bg = None
    first_any = None
    for i, ev in enumerate(events):
        if ev["event"] == "client_first_frame":
            if first_any is None:
                first_any = i
            if ev.get("role") == "background" and first_bg is None:
                first_bg = i
    if first_bg is None:
        raise AssertionError(
            "no client_first_frame{role:background} in the halmasuit "
            "event stream — splash never composited a background frame.\n"
            f"events seen: {[e['event'] for e in events]}"
        )
    # A background cff is itself an "any" cff, so first_any is set and
    # first_any <= first_bg.
    assert first_any is not None

    cov_viol = [
        (i, ev)
        for i, ev in enumerate(events)
        if i >= first_bg
        and ev["event"] == "frame_rendered"
        and ev["backdrop_coverage"] <= 0.95
    ]
    lum_viol = [
        (i, ev)
        for i, ev in enumerate(events)
        if i >= first_any
        and ev["event"] == "frame_rendered"
        and ev["mean_luminance"] < 0.01
    ]
    frames = sum(1 for e in events if e["event"] == "frame_rendered")
    if cov_viol or lum_viol:
        raise AssertionError(
            "FrameRendered continuity invariant VIOLATED "
            f"({frames} frames; first bg cff @#{first_bg}, "
            f"first any cff @#{first_any}):\n"
            f"  backdrop_coverage<=0.95 after bg: {cov_viol[:5]}\n"
            f"  mean_luminance<0.01 after first commit: {lum_viol[:5]}"
        )
    print(
        json.dumps(
            {
                "continuity": "OK",
                "frames": frames,
                "first_background_cff_index": first_bg,
            }
        )
    )
