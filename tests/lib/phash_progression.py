"""Phash-progression assertion for animating-wallpaper VM tests.

A frame-stream is "animating" if successive `Event::FrameRendered`
events carry MEASURABLY DIFFERENT perceptual hashes (`phash`,
halmasuit-introspect's 8x8 average-hash u64). A frozen wallpaper —
the failure mode this utility exists to catch — emits the same phash
on every frame because the render path is producing the same pixels
even though the wallpaper backend (shader iTime, video decoder)
should be advancing.

Why both `min_distinct_phashes` AND `min_hamming_max` thresholds:
two distinct classes of frozenness can occur and each needs its own
gate.

  * `min_distinct_phashes`: catches the obvious "every frame is
    identical" case. A frozen shader iTime, a frozen video decoder,
    a paused DrmDevice all hit this — len(set(phashes)) == 1.
    Default 5: even a slow animation (60s-period sine on R channel)
    crossing a few render cycles produces ≥5 distinct hash values
    in practice.

  * `min_hamming_max`: catches "subtle drift" frozenness where
    floating-point jitter or LSB noise produces nominally-distinct
    phashes that are perceptually identical. Without this, an
    animation that is FROZEN visually but oscillates between two
    near-identical phashes (max hamming 1-2 bits / 64) would pass
    the distinct-count check. Default 10: requires at least one
    pair of phashes in the window to differ by ≥10 bits, which is
    well above noise but reachable by any real animation change
    (the shader fixture's R-channel hue rotation alone moves
    ~20 bits over a few seconds).

Consumers:
  * R3.3 `visual-shutdown-shader.nix` — asserts shader keeps
    animating across the shutdown window
  * R3.5 `visual-shutdown-video.nix` — asserts video decoder keeps
    producing distinct frames across the shutdown window
  * R3.7 Phase B retrofit — closes the existing "frozen shader/video
    would pass today" gap in visual-phase-b-{side,enc}-{shader,video}

The utility is intentionally pure-Python with no halmasuit-specific
imports — it consumes the dict-shaped event stream
`visual.introspect_events()` returns. Self-test at the bottom
(`python3 tests/lib/phash_progression.py`) covers the three
canonical failure / success shapes.
"""

from typing import Any, Callable, Optional


def hamming_u64(a: int, b: int) -> int:
    """Number of differing bits in two 64-bit unsigned integers.

    For perceptual hashes (phash) the Hamming distance is a coarse
    proxy for perceptual difference: identical images = 0 bits,
    completely-unrelated 8x8-average-hashed images ≈ 32 bits
    (random over 64). Real-world thresholds: ≤ 5 bits = visually
    identical, ≥ 10 bits = visually distinct.
    """
    if a < 0 or b < 0 or a >> 64 or b >> 64:
        raise ValueError(f"phash must be in u64 range: a={a}, b={b}")
    return bin(a ^ b).count("1")


def assert_animating(
    events: list[dict[str, Any]],
    *,
    window_filter: Optional[Callable[[dict[str, Any]], bool]] = None,
    min_distinct_phashes: int = 5,
    min_hamming_max: int = 10,
) -> None:
    """Assert the frame_rendered subset of `events` shows animation.

    Args:
        events: List of dicts as returned by `visual.introspect_events(machine)`.
            Only entries with `event == "frame_rendered"` and a `phash`
            field are considered; everything else is ignored.
        window_filter: Optional `callable(event_dict) -> bool` applied
            BEFORE the frame_rendered filter, so the caller can restrict
            the window (e.g. "only frames after the post-pivot marker").
            Default None = no extra filtering.
        min_distinct_phashes: Minimum number of distinct phash values
            required in the window. Catches "every frame identical"
            frozenness. Default 5.
        min_hamming_max: Minimum value of the maximum pairwise Hamming
            distance between phashes in the window. Catches "low-bit-drift"
            frozenness where phashes are nominally different but
            perceptually identical. Default 10 (out of 64).

    Raises:
        AssertionError: If either threshold is violated. The message
            includes: total frame count, distinct phash count, max
            pairwise Hamming distance, the last 10 phashes observed
            (hex), and which specific threshold failed.
    """
    if window_filter is not None:
        events = [e for e in events if window_filter(e)]
    frame_events = [
        e
        for e in events
        if e.get("event") == "frame_rendered" and "phash" in e
    ]
    if not frame_events:
        raise AssertionError(
            "phash_progression: 0 frame_rendered events in window — "
            "halmasuit-introspect's frame_audit stream is empty. The "
            "halmasuit-debug binary (with the `frame_audit` feature) "
            "is required; production halmasuit emits no FrameRendered."
        )

    phashes = [int(e["phash"]) for e in frame_events]
    distinct = sorted(set(phashes))

    max_hamming = 0
    if len(distinct) >= 2:
        # O(n²) but n is small (typically <100 across a test window).
        # Two-pass: outer over distinct values, inner over later ones.
        for i, a in enumerate(distinct):
            for b in distinct[i + 1 :]:
                d = hamming_u64(a, b)
                if d > max_hamming:
                    max_hamming = d

    last_n = phashes[-10:]
    last_n_hex = ", ".join(f"0x{p:016x}" for p in last_n)

    if len(distinct) < min_distinct_phashes:
        raise AssertionError(
            f"phash_progression: only {len(distinct)} distinct phash(es) "
            f"across {len(frame_events)} frame_rendered events "
            f"(required ≥ {min_distinct_phashes}). The wallpaper is "
            f"FROZEN — render path is either paused or producing "
            f"identical pixels every frame.\n"
            f"  last {len(last_n)} phashes: {last_n_hex}"
        )

    if max_hamming < min_hamming_max:
        raise AssertionError(
            f"phash_progression: max pairwise Hamming distance "
            f"{max_hamming} < {min_hamming_max} across "
            f"{len(distinct)} distinct phashes (low-bit-drift "
            f"frozenness — nominally distinct but perceptually "
            f"identical).\n"
            f"  last {len(last_n)} phashes: {last_n_hex}"
        )


if __name__ == "__main__":
    # Self-test: exercise the three canonical failure / success shapes.
    # Run as `python3 tests/lib/phash_progression.py`.

    def _frame(phash: int):
        return {"event": "frame_rendered", "phash": phash}

    # Case 1: all phashes identical → frozen, FAILS on distinct-count.
    frozen = [_frame(0xDEAD_BEEF_CAFE_F00D) for _ in range(20)]
    try:
        assert_animating(frozen)
    except AssertionError as e:
        msg = str(e)
        assert "only 1 distinct phash" in msg, f"unexpected msg: {msg}"
        print("[ok] frozen stream → AssertionError on distinct-count")
    else:
        raise SystemExit("BUG: all-identical phashes should have failed")

    # Case 2: 6 distinct phashes but all within 1-2 bits of each other →
    # passes distinct-count, FAILS on hamming-max (perceptually frozen).
    base = 0xAAAA_AAAA_AAAA_AAAA
    drifty = [_frame(base ^ (1 << (i % 3))) for i in range(20)]
    # Force ≥ min_distinct_phashes (5) by using 6 distinct LSB-perturbations.
    drifty = [_frame(base ^ (1 << i)) for i in range(6)] + drifty
    try:
        assert_animating(drifty)
    except AssertionError as e:
        msg = str(e)
        assert "max pairwise Hamming distance" in msg, f"unexpected msg: {msg}"
        print("[ok] low-bit-drift stream → AssertionError on hamming-max")
    else:
        raise SystemExit("BUG: low-bit-drift phashes should have failed")

    # Case 3: healthy animation — 10 distinct phashes, well-separated
    # in Hamming space. Passes both thresholds.
    healthy = [
        _frame(0x0000_0000_0000_0000),
        _frame(0xFFFF_FFFF_FFFF_FFFF),  # 64-bit Hamming distance from previous
        _frame(0x1234_5678_9ABC_DEF0),
        _frame(0xFEDC_BA98_7654_3210),
        _frame(0xAAAA_5555_AAAA_5555),
        _frame(0x5555_AAAA_5555_AAAA),
        _frame(0xCAFE_BABE_DEAD_BEEF),
        _frame(0x1357_9BDF_2468_ACE0),
        _frame(0xF0F0_F0F0_0F0F_0F0F),
        _frame(0x8000_0000_0000_0001),
    ]
    assert_animating(healthy)
    print("[ok] healthy animation stream → PASSES")

    # Case 4: window_filter narrows the input correctly.
    mixed = healthy + frozen  # 10 healthy then 20 frozen
    # Filter: only events where phash is in our healthy set.
    healthy_phashes = {int(e["phash"]) for e in healthy}
    assert_animating(mixed, window_filter=lambda e: int(e.get("phash", -1)) in healthy_phashes)
    print("[ok] window_filter correctly narrows event set")

    # Case 5: empty input → explicit error, not silent pass.
    try:
        assert_animating([])
    except AssertionError as e:
        msg = str(e)
        assert "0 frame_rendered events" in msg, f"unexpected msg: {msg}"
        print("[ok] empty events → AssertionError")
    else:
        raise SystemExit("BUG: empty events should have failed")

    # Case 6: hamming_u64 sanity.
    assert hamming_u64(0, 0) == 0
    assert hamming_u64(0xFFFF_FFFF_FFFF_FFFF, 0) == 64
    assert hamming_u64(0xAAAA_AAAA_AAAA_AAAA, 0x5555_5555_5555_5555) == 64
    assert hamming_u64(0x0000_0000_0000_0001, 0x0000_0000_0000_0000) == 1
    print("[ok] hamming_u64 sanity")

    print("phash_progression: all self-tests passed.")
