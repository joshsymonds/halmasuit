# tests/lib/shutdown_testscript.py — shared assertion body for the
# wallpaper-shutdown matrix cells.
#
# Used by:
#   - tests/visual-shutdown-pivot-survival.nix  (shader cell)
#   - tests/visual-shutdown-image.nix           (image cell)
#   - tests/visual-shutdown-video.nix           (video cell)
#
# Matrix-shared invariants this body asserts (always):
#   1. `Successfully changed into root pivot` marker reached — proves
#      systemd-shutdown actually pivoted into the shutdownRamfs.
#   2. At least one `halmasuit-shutdown-liveness pid=N` line AFTER
#      the post-pivot marker — proves halmasuit's same PID survived
#      the rootfs→shutdownRamfs transition.
#   3. PID continuity: every liveness line in the entire console
#      carries the SAME pid, and it matches the pre-shutdown
#      halmasuit MainPID. Different pid = process respawned.
#   4. No `coredump:` kernel log for halmasuit's PID — the #60
#      double-panic-abort regression hasn't returned.
#
# Per-cell optional extras (parameters of `run()`):
#   - `pre_shutdown_hook(machine, halmasuit_pid)`: runs after
#     niri is up and before `machine.shutdown()`. Image cell uses
#     this to run `visual.assert_matches_exact(machine, "...")`
#     against its committed golden.
#   - `assert_frame_advancement` (bool): assert `frames=N` field on
#     liveness lines strictly increases across the shutdown window
#     (Reached target System Power Off → end of trace). Shader and
#     video cells enable; image disables (no animation work to drive).
#   - `phash_min_distinct` + `phash_min_hamming_max` (int | None):
#     if BOTH are provided, run `phash_progression.assert_animating()`
#     against the captured console JSON events with these thresholds.
#     Shader cell uses (3, 8); video uses (3, 20) — testsrc's 8x8
#     phash quantizes to ~5-6 buckets but with large Hamming spread.
#     Image cell passes None for both (no animation).
#   - `wait_for_nonzero_phash` (bool): before machine.shutdown(),
#     poll the journal for a `frame_rendered` event whose `phash`
#     field is non-zero. Defeats a startup race observed in the
#     video cell: if the decoder relay hasn't produced its first
#     non-black frame before shutdown begins, the captured event
#     window can contain only all-zero phashes and the
#     phash-progression assertion sees < min_distinct buckets.
#     Video cell enables; shader cell doesn't need it (the shader
#     renders synchronously and the first frame already varies);
#     image cell doesn't need it (no animation).
#
# The session_cmd path is interpolated at Nix evaluation time and
# passed in by each cell; this module never imports Nix paths.

from __future__ import annotations

import re
from typing import Any, Callable

import phash_progression


def run(
    machine: Any,
    *,
    cell_name: str,
    session_cmd: str,
    pre_shutdown_hook: Callable[[Any, str], None] | None = None,
    assert_frame_advancement: bool = False,
    phash_min_distinct: int | None = None,
    phash_min_hamming_max: int | None = None,
    wait_for_nonzero_phash: bool = False,
) -> None:
    """Drive the boot → auth → niri → shutdown sequence and assert
    the matrix-shared survival invariants. See module docstring.
    """
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame",
        timeout=30,
    )

    # ── Auth + niri session up ─────────────────────────────────────
    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        f"--cmd {session_cmd} "
        "--timeout 20"
    )
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    print(f"PASS: halmasuit pid {halmasuit_pid} with niri session up")

    # ── Optional: wait for first non-zero phash before shutdown ───
    # Video cell's decoder relay produces its first decoded frame
    # asynchronously after halmasuit comes up; if shutdown is
    # triggered before that, the captured event window may contain
    # only all-zero phashes (frame_rendered for the placeholder
    # black framebuffer) and the phash-progression assertion sees
    # < min_distinct buckets. Polling the journal for at least one
    # `frame_rendered ... phash=<non-zero>` line converts this race
    # into a deterministic gate.
    if wait_for_nonzero_phash:
        # halmasuit's tracing output wraps the event JSON inside an
        # outer envelope: `"json":"{\"event\":\"frame_rendered\",
        # ...,\"phash\":<int>}"`. The inner `phash` field is
        # escaped-JSON-in-JSON, so the literal text in journalctl
        # output is `\"phash\":<digits>`. Match any non-zero phash:
        # exclude `\"phash\":0\b` and require a digit follows.
        machine.wait_until_succeeds(
            "journalctl -u halmasuit -o cat "
            "| grep -E 'frame_rendered.*\\\\\"phash\\\\\":[1-9]' "
            "| head -n 1 | grep -q .",
            timeout=30,
        )
        print(
            f"PASS: at least one frame_rendered event with non-zero phash "
            f"observed before shutdown ({cell_name})"
        )

    # ── Optional pre-shutdown assertion (e.g. SSIMULACRA2 golden) ──
    if pre_shutdown_hook is not None:
        pre_shutdown_hook(machine, halmasuit_pid)
        print(f"PASS: pre-shutdown hook completed for {cell_name}")

    # ── Trigger full system shutdown ────────────────────────────────
    # `machine.shutdown()` issues `systemctl poweroff` over the
    # backdoor and waits for the QEMU process to exit. After it
    # returns, the serial console captures everything systemd-shutdown
    # logged — including the post-pivot binary's output and any
    # kmsg lines halmasuit wrote via its journal+kmsg-routed stdout.
    machine.shutdown()
    console = machine.get_console_log()

    # ── Locate the post-pivot marker ────────────────────────────────
    # `Successfully changed into root pivot` is logged by systemd-
    # shutdown the instant after it execve's into the shutdown
    # initramfs — THE post-pivot moment. Any halmasuit liveness line
    # after it proves the same PID survived the rootfs→shutdownRamfs
    # transition.
    pivot_re = re.compile(r"Successfully changed into root pivot", re.MULTILINE)
    pivot_match = pivot_re.search(console)
    if pivot_match is None:
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate `Successfully changed into root pivot` "
            "marker in serial console. systemd-shutdown either did "
            "not complete the pivot or the log line did not make it "
            "to the console.\n\nLast 100 console lines:\n" + tail
        )
    pivot_offset = pivot_match.start()
    pivot_line = console.count("\n", 0, pivot_offset)
    print(f"PASS: located post-pivot marker at console line {pivot_line}")

    # ── Heartbeat-after-pivot assertion ────────────────────────────
    # halmasuit's always-on liveness timer writes
    # `halmasuit-shutdown-liveness pid=N frames=M` every
    # HALMASUIT_LIVENESS_INTERVAL_MS to stdout. Production has
    # `StandardOutput=file:/dev/kmsg`, which makes systemd open
    # /dev/kmsg directly and pass the fd to halmasuit — bypassing
    # the journald-stdout pipe, so the bytes still reach the kernel
    # ring buffer after systemd-journald is killed mid-shutdown and
    # across the rootfs→shutdownRamfs pivot.
    hb_re = re.compile(r"halmasuit-shutdown-liveness pid=(\d+) frames=(\d+)")

    post_pivot = console[pivot_offset:]
    post_pivot_hbs = list(hb_re.finditer(post_pivot))
    if not post_pivot_hbs:
        post_pivot_window = "\n".join(post_pivot.splitlines()[:80])
        raise AssertionError(
            "Production halmasuit did NOT survive the rootfs→"
            "shutdownRamfs pivot: 0 `halmasuit-shutdown-liveness` "
            "kmsg lines after the `Successfully changed into root "
            "pivot` marker. Either SurviveFinalKillSignal=yes "
            "regressed, or the binary isn't in shutdownRamfs's "
            "storePaths, or graceful_shutdown started exiting the "
            "process instead of letting the loop continue."
            "\n\nFirst 80 lines after the marker:\n"
            + post_pivot_window
        )
    last_post = post_pivot_hbs[-1]
    print(
        f"PASS: {len(post_pivot_hbs)} halmasuit-shutdown-liveness line(s) "
        f"emitted AFTER the post-pivot marker; last pid={last_post.group(1)}, "
        f"last frames={last_post.group(2)}"
    )

    # ── Optional: render-counter advancement (shader/video) ────────
    # The `frames=N` field on the liveness line is halmasuit's
    # always-on render counter (DrmBackend::frame_counter), bumped on
    # every successful `render_one_frame`. Asserting it strictly
    # increases ACROSS THE SHUTDOWN WINDOW proves the wallpaper-engine
    # tick is actually advancing the render path through shutdown.
    #
    # Why the wider window (System Power Off → end of trace, not just
    # post-pivot): the wallpaper-tick fires at 100ms cadence, but the
    # post-pivot slice is typically only ~30-100ms before kernel
    # halt — too narrow to reliably capture multiple ticks.
    if assert_frame_advancement:
        shutdown_start_re = re.compile(
            r"Reached target System Power Off", re.MULTILINE
        )
        shutdown_start_match = shutdown_start_re.search(console)
        if shutdown_start_match is None:
            raise AssertionError(
                "Could not locate `Reached target System Power Off` marker "
                "for the shutdown-window frame-advancement assertion."
            )
        shutdown_window_start = shutdown_start_match.start()
        shutdown_hbs = list(hb_re.finditer(console[shutdown_window_start:]))
        shutdown_frames = [int(m.group(2)) for m in shutdown_hbs]
        if len(shutdown_frames) < 2:
            raise AssertionError(
                f"Need at least 2 post-System-Power-Off liveness lines to "
                f"assert frame progression; got {len(shutdown_frames)}. "
                f"Either HALMASUIT_LIVENESS_INTERVAL_MS is too high or the "
                f"shutdown window is too short."
            )
        first_frames, last_frames = shutdown_frames[0], shutdown_frames[-1]
        delta = last_frames - first_frames
        if delta <= 0:
            raise AssertionError(
                f"halmasuit's render counter did NOT advance across the "
                f"shutdown window: first={first_frames}, last={last_frames}. "
                f"The wallpaper-engine tick is not driving renders through "
                f"shutdown — shader/video wallpaper would freeze. "
                f"All shutdown-window frame counts: {shutdown_frames}"
            )
        print(
            f"PASS: render counter advanced across shutdown window from "
            f"{first_frames} to {last_frames} (+{delta} frames across "
            f"{len(shutdown_frames)} liveness samples). The wallpaper-engine "
            f"tick is driving renders through shutdown."
        )

    # ── Optional: phash progression (shader/video) ─────────────────
    # Beyond "the counter is advancing", prove the RENDERED PIXELS
    # are actually different across frames. frame_audit emits
    # `Event::FrameRendered` per frame via tracing's JSON formatter
    # on stderr → journald → /dev/console. We can't call
    # `visual.introspect_events(machine)` after machine.shutdown()
    # because that would `journalctl` against a powered-off VM;
    # parse the same JSON envelopes out of the captured console.
    if phash_min_distinct is not None and phash_min_hamming_max is not None:
        all_events = phash_progression.events_from_console(console)
        phash_progression.assert_animating(
            all_events,
            min_distinct_phashes=phash_min_distinct,
            min_hamming_max=phash_min_hamming_max,
        )
        frame_rendered_events = [
            e for e in all_events
            if e.get("event") == "frame_rendered" and "phash" in e
        ]
        distinct_phashes = {int(e["phash"]) for e in frame_rendered_events}
        print(
            f"PASS: phash progression — {len(distinct_phashes)} distinct "
            f"phashes across {len(frame_rendered_events)} frame_rendered "
            f"events. The wallpaper is animating (not frozen)."
        )

    # ── No-coredump assertion ──────────────────────────────────────
    coredump_re = re.compile(rf"coredump:\s+{halmasuit_pid}\(halmasuit\)")
    cd = coredump_re.search(console)
    if cd:
        raise AssertionError(
            f"halmasuit PID {halmasuit_pid} took a coredump-class signal "
            f"during shutdown — the regression #60 fixed has returned. "
            f"Match: {console[max(0, cd.start() - 80):cd.end() + 80]}"
        )
    print("PASS: no coredump for halmasuit MainPID throughout shutdown")

    # ── PID continuity assertion ───────────────────────────────────
    # Every liveness line (both pre- and post-pivot) must carry the
    # SAME pid, and it must match the halmasuit MainPID we captured
    # before shutdown. Different pid post-pivot = process respawned,
    # not survived — which is the regression we're guarding against.
    all_hbs = list(hb_re.finditer(console))
    pids = {m.group(1) for m in all_hbs}
    if len(pids) != 1:
        raise AssertionError(
            f"halmasuit-shutdown-liveness lines carry MULTIPLE pids: "
            f"{sorted(pids)}. The compositor respawned mid-shutdown "
            "(lost SurviveFinalKillSignal contract) or another process "
            "is impersonating its liveness lines."
        )
    surviving_pid = next(iter(pids))
    if surviving_pid != halmasuit_pid:
        raise AssertionError(
            f"halmasuit-shutdown-liveness lines carry pid={surviving_pid} "
            f"but the pre-shutdown halmasuit MainPID was {halmasuit_pid}. "
            "PID continuity invariant violated — the compositor we see "
            "post-pivot is NOT the same one that was painting the "
            "wallpaper pre-shutdown."
        )
    print(
        f"PASS: every halmasuit-shutdown-liveness line (pre- and post-pivot, "
        f"{len(all_hbs)} total) carries pid={surviving_pid} — same PID "
        f"throughout shutdown"
    )

    print(f"visual-{cell_name}: ALL ASSERTIONS PASSED")
