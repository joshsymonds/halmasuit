"""Visual-test helpers for halmasuit NixOS tests.

Importable from a `testScript` via::

    import sys
    sys.path.insert(0, "${./lib}")
    import visual

The module wraps three concerns:

  * `capture(machine, name)` — `machine.screenshot(name)` plus host-side
    path resolution. The NixOS test driver writes screenshots into its
    output directory (`${out}` for store-built tests; `/tmp/...` for
    interactive runs).

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


def capture(machine, name: str) -> Path:
    """Take a screenshot and return the host-side PNG path.

    `machine.screenshot(name)` writes into the NixOS test driver's
    output directory. The driver also exports the output directory via
    the `out` env var when running under `nix build`; fall back to
    common locations.
    """
    machine.screenshot(name)
    out_dir = os.environ.get("out") or os.environ.get("TMPDIR") or "/tmp"
    candidate = Path(out_dir) / f"{name}.png"
    if candidate.exists():
        return candidate
    for d in (out_dir, "/tmp", "."):
        p = Path(d) / f"{name}.png"
        if p.exists():
            return p
    raise FileNotFoundError(
        f"screenshot {name!r} not found after machine.screenshot(); "
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
