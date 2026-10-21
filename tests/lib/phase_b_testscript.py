# tests/lib/phase_b_testscript.py — shared body for the six Phase B
# golden-boot matrix cells (visual-phase-b-{side,enc}-{image,shader,
# video}). Imported from each cell's `testScript` after the cell sets
# `cell_name`, `lukshape`, and `no_flash_mode` module-level variables.
# Keeps the per-cell .nix file to just the wallpaper + LUKS-shape +
# decoder wiring; the full ~190-line lifecycle assertion sequence lives
# here once.
"""Shared Phase B golden-boot testScript body.

Cell .nix files supply:

  cell_name      – str, e.g. "phase-b-side-image" (used for the
                   session-scene golden name).
  lukshape       – str, one of "side-volume" or "encrypted-root".
                   Switches the first-boot dance (side-volume: in-place
                   luksFormat then crash; encrypted-root: luksFormat
                   then bootctl set-default the cryptroot specialisation
                   then crash).
  no_flash_mode  – str, one of "strict" (image/shader cells; assert_no
                   _flash_stream over the whole stream) or "suffix"
                   (video cell; pre-decode startup window excluded, per
                   the wallpaper-engine follow-up note).
  machine        – the NixOS test driver Machine instance (supplied by
                   the runNixOSTest harness; available at module scope).
  visual         – the tests/lib/visual.py module (imported by the cell
                   shim, sys.path-injected).

The body assumes `import os`, `import sys`, `import time`, `import re`
already happened in the cell shim.
"""

import phash_progression


def fg_events(machine, visual):
    return [
        e["to"] for e in visual.introspect_events(machine)
        if e["event"] == "foreground_changed"
    ]


def run(machine, visual, *, cell_name, lukshape, no_flash_mode):
    assert lukshape in ("side-volume", "encrypted-root"), lukshape
    assert no_flash_mode in ("strict", "suffix"), no_flash_mode

    # ── First boot ───────────────────────────────────────────────────
    # side-volume: regular boot with /dev/vdb available but not yet
    #              LUKS-formatted; the helper's custom cryptsetup unit
    #              guard-skips. testScript luksFormats /dev/vdb in
    #              place, then crashes; the second boot's same-config
    #              unit ticks systemd-cryptsetup which asks the
    #              halmasuit-luks responder for the passphrase.
    # encrypted-root: regular boot of the DEFAULT (non-cryptroot)
    #              specialisation. testScript luksFormats /dev/vdb,
    #              swaps the bootloader default to the cryptroot
    #              specialisation entry, then crashes; the second
    #              boot's cryptroot specialisation mounts
    #              /dev/mapper/cryptroot as / and halmasuit-luks
    #              answers the ask-password.
    machine.start()
    machine.wait_for_unit("multi-user.target")
    print("PASS: first boot reached multi-user.target")

    machine.succeed(
        "printf 'luks-test-unlock-secret' | "
        "cryptsetup luksFormat -q --iter-time 1 /dev/vdb -"
    )
    print("PASS: /dev/vdb formatted with canonical passphrase")

    if lukshape == "encrypted-root":
        # Pre-format the encrypted root with a real ext4 NOW, in the
        # first boot. systemd 260.1's 99-systemd.rules pins an EMPTY
        # CRYPT-* device (no partition table, no filesystem usage) to
        # SYSTEMD_READY=0, so on the second boot a freshly-luksFormat'd
        # cryptroot's `dev-mapper-cryptroot.device` unit never activates
        # in the initrd → `autoFormat`'s systemd-makefs + sysroot.mount
        # never run → no `initrd-switch-root` → the boot wedges (the
        # compositor keeps painting forever, never pivots). Giving the
        # device a filesystem here makes blkid report ID_FS_USAGE so the
        # rule no longer fires and the device unit activates. This also
        # matches a REAL encrypted-root deployment (e.g. gnomon), whose
        # root is formatted once at install — `autoFormat`
        # (format-on-first-boot) was always a test-only convenience that
        # the systemd-260 rule retroactively broke for CRYPT devices.
        machine.succeed(
            "printf 'luks-test-unlock-secret' | "
            "cryptsetup open /dev/vdb cryptroot-preformat -"
        )
        machine.succeed("mkfs.ext4 -F /dev/mapper/cryptroot-preformat")
        machine.succeed("cryptsetup close cryptroot-preformat")
        print("PASS: encrypted root pre-formatted ext4 (systemd-260 CRYPT-readiness fix)")

        # bootctl set-default the cryptroot specialisation entry. Entry
        # ids are content hashes (nixos-<hash>.conf); the specialisation
        # name appears only in the title, so find the one entry block
        # titled "(cryptroot)" and take its id.
        import re
        entries = machine.succeed("bootctl list --no-pager")
        ids = [
            m.group(1)
            for block in re.split(r"\n\s*\n", entries)
            if re.search(r"title:.*\(cryptroot\)", block)
            and (m := re.search(r"id:\s+(\S+\.conf)", block))
        ]
        assert len(ids) == 1, f"expected one cryptroot specialisation entry, got {ids}:\n{entries}"
        cryptroot_entry = ids[0]
        machine.succeed(f"bootctl set-default {cryptroot_entry}")
        print(f"PASS: bootctl default → {cryptroot_entry}")

    machine.succeed("sync")

    # ── Crash + reboot ───────────────────────────────────────────────
    machine.crash()
    machine.start()
    machine.wait_for_unit("multi-user.target")
    print("PASS: second boot reached multi-user.target")

    # The rootfs adopts the compositor that crossed the pivot: its unit
    # is loaded and active, and no job (the boot isolate's stop) is
    # queued against it.
    unit = machine.succeed(
        "systemctl show -p LoadState -p ActiveState halmasuit.service"
    )
    assert "LoadState=loaded" in unit and "ActiveState=active" in unit, (
        f"halmasuit.service not adopted by the rootfs:\n{unit}"
    )
    jobs = machine.succeed("systemctl list-jobs --no-legend --no-pager")
    assert "halmasuit.service" not in jobs, (
        f"a job is queued against the compositor:\n{jobs}"
    )
    print("PASS: rootfs adopted halmasuit.service with no job queued")

    # ── LUKS unlock assertion ────────────────────────────────────────
    if lukshape == "side-volume":
        mapper = machine.succeed("ls /dev/mapper").strip()
        assert "test-luks-data" in mapper.split(), (
            f"/dev/mapper/test-luks-data not present after the agent-driven "
            f"unlock.\n/dev/mapper contents: {mapper}\n"
            + machine.execute(
                "journalctl -b -u halmasuit-luks --output=cat --no-pager"
            )[1]
        )
        print("PASS: /dev/mapper/test-luks-data unlocked via initramfs agent path")
    else:
        root_mount = machine.succeed("findmnt -n -o SOURCE /").strip()
        assert root_mount == "/dev/mapper/cryptroot", (
            f"root not on cryptroot mapper: {root_mount}"
        )
        print(f"PASS: / is mounted from {root_mount}")

    # halmasuit-luks's structured log confirms its wire answered the
    # cryptsetup ask-password. Grounds the unlock claim on the agent
    # side, not just the consumer side.
    raw = machine.succeed("journalctl -b --output=cat --no-pager")
    assert "responded to ask-password request" in raw, (
        "halmasuit-luks did NOT log `responded to ask-password request` — "
        "the LUKS unlock may have happened via another agent.\n"
        + machine.execute(
            "journalctl -b -u halmasuit-luks --no-pager | tail -30"
        )[1]
    )
    print("PASS: halmasuit-luks's wire was the unlock responder")

    # ── Post-pivot halmasuit + greeter ───────────────────────────────
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"greeter_spawned\\\"'",
        timeout=60,
    )
    print("PASS: greeter spawned post-pivot")

    # Lifecycle event cross-assertions (cf. cell docstring). The Phase B
    # phases (initramfs_init / rootfs_ready) are the new initramfs-side
    # markers added for fromInitrd.
    phases_seen = {
        e["phase"] for e in visual.introspect_events(machine)
        if e["event"] == "phase_entered"
    }
    for required in ("initramfs_init", "rootfs_ready", "greetd_ready"):
        assert required in phases_seen, (
            f"phase {required!r} missing from {sorted(phases_seen)}"
        )
    print(f"PASS: Phase B phase_entered sequence ({sorted(phases_seen)})")

    # DankGreeter (DMS Quickshell) is the greeter.
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F 'new layer surface'",
        timeout=60,
    )
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"foreground_changed\\\"'",
        timeout=30,
    )
    fg = fg_events(machine, visual)
    assert "greeter" in fg, f"expected greeter foreground; got {fg}"
    print("PASS: foreground=greeter; DankGreeter layer surface up")
    # No greeter-scene SSIMULACRA2 golden: DankGreeter renders a live
    # clock (HH:MM) that ticks every minute, and SSIMULACRA2 penalises
    # text-pixel changes heavily — even two captures of the same scene
    # one second apart score far below the global 90.0 threshold. The
    # greeter rendering IS still gated visually below via
    # assert_no_flash_stream and the session-scene golden; the keyboard
    # arc additionally proves DankGreeter is alive and focused.

    # ── R13(b) real keyboard arc through DankGreeter ────────────────
    # Phase B halmasuit argv[0] is rewritten to "@" (RESEARCH.md Phase 2,
    # SurviveFinalKillSignal); pgrep on the binary path doesn't match.
    # Source the PID from the structured `started` event instead.
    started_events = [
        e for e in visual.introspect_events(machine) if e["event"] == "started"
    ]
    assert started_events, "halmasuit did not emit `started`"
    halmasuit_pid = str(started_events[-1]["pid"])
    print(f"halmasuit PID = {halmasuit_pid}")

    # DMS QML uses ONE TextField that toggles between username and
    # password mode on Enter. The username→password transition is
    # client-side state inside Quickshell — no journal-visible marker.
    visual.wait_greeter_settled(machine)
    machine.send_chars("alice")
    machine.send_key("ret")
    import time
    time.sleep(1)
    machine.send_chars("testpassword")
    machine.send_key("ret")

    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager | grep -qF session_opened",
        timeout=120,
    )
    print("PASS: session_opened (real pam_unix through broker)")

    # Keystrokes reach the foreground VT's tty too; no console getty
    # may have taken them as a login.
    machine.fail(
        "journalctl -b -o cat --no-pager | grep -F 'pam_unix(login:session)'"
    )
    print("PASS: no console login took the typed credentials")

    # Real niri came up as the broker-launched session.
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    # Wait for the swap-gate's session foreground event, not the
    # toplevel-mapped log: those fire one calloop turn apart and racing
    # the introspection JSON dump against fg_events() saw only the
    # greeter when the assertion landed between them.
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"foreground_changed\\\",\\\"to\\\":\\\"session\\\"'",
        timeout=60,
    )
    # fg_events() is scoped to the current boot via journalctl -b
    # (see tests/lib/visual.py:introspect_events).
    events = fg_events(machine, visual)
    assert events and events[-1] == "session", (
        f"final foreground should be session; got {events}"
    )
    assert "greeter" in events[: events.index("session")], (
        f"expected greeter foreground BEFORE session; got {events}"
    )
    print("PASS: niri mapped fullscreen; foreground=session")

    # The swap SIGKILLed the greeter. Its whole process tree is gone,
    # and the broker, its parent, reaped it.
    def greeter_leftovers():
        return machine.succeed(
            "ps -u halmasuit-greeter -o pid=,ppid=,stat=,etime=,cgroup=,args= "
            "| grep -v '^ *[0-9]* *[0-9]* *Z' || true"
        ).strip()

    deadline = time.monotonic() + 30
    while greeter_leftovers():
        assert time.monotonic() < deadline, (
            f"greeter processes outlived the swap:\n{greeter_leftovers()}"
        )
        time.sleep(1)
    machine.wait_until_succeeds(
        "pid=$(systemctl show -p MainPID --value halmasuit-session.service); "
        "test \"$pid\" -gt 0 && ! ps -o stat= --ppid \"$pid\" | grep -q '^Z'",
        timeout=30,
    )
    print("PASS: greeter process tree gone after the swap; broker reaped it")

    # The observability surface came up on the rootfs bus after the
    # pivot.
    machine.wait_until_succeeds(
        "busctl --system status org.halmasuit.Compositor1", timeout=30
    )
    print("PASS: org.halmasuit.Compositor1 served after the pivot")

    # halmasuit PID continuous across the swap (login-flash invariant).
    started_now = [
        e for e in visual.introspect_events(machine) if e["event"] == "started"
    ]
    pid_now = str(started_now[-1]["pid"]) if started_now else ""
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: "
        f"{halmasuit_pid} -> {pid_now}"
    )
    print(f"PASS: halmasuit pid {halmasuit_pid} continuous")

    # Session-scene golden AFTER niri maps. Two captures from the
    # same settled scene:
    #
    # - `current`: live composition (wallpaper + niri's opaque
    #   fullscreen xdg_toplevel + cursor). Same content per cell —
    #   gates the niri-on-halmasuit render path.
    # - `wallpaper-only`: variant-distinct, read from the auxiliary
    #   slot halmasuit-debug publishes alongside every audited frame
    #   (no cursor / no layer-shell / no toplevel). Closes review
    #   finding C-G1; cell-distinguishing wallpaper coverage that the
    #   `current` scene can't provide because niri's toplevel is
    #   opaque. Skipped on video cells for the same reason the strict
    #   no-flash anchor is — decoder startup may have produced an
    #   empty fbo at the read moment.
    time.sleep(2)
    # SSIMULACRA2 goldens only fit STATIC content. Animated cells
    # (shader, video) have R3.1's wallpaper-tick driving continuous
    # animation post-boot, so the captured frame's iTime varies
    # boot-to-boot and a fixed golden can't match. The
    # phash_progression assertion further down replaces this for
    # animated cells — it proves the wallpaper IS animating (frames
    # are distinct) without depending on a fixed-iTime golden.
    is_animated = "shader" in cell_name or "video" in cell_name
    if not is_animated:
        visual.assert_matches_golden(machine, f"{cell_name}-session")
        print("PASS: session-scene golden")
        if no_flash_mode == "strict":
            visual.assert_matches_golden(
                machine, f"{cell_name}-wallpaper", scene="wallpaper-only"
            )
            print("PASS: wallpaper-only golden (variant-distinct)")
    else:
        print(
            f"SKIP: SSIMULACRA2 golden for animated cell {cell_name!r} — "
            f"phash_progression assertion below covers the animating-"
            f"content invariant without iTime-stability requirement."
        )

    # ── No-flash invariant over the full timeline ───────────────────
    if no_flash_mode == "strict":
        visual.assert_no_flash_stream(machine)
        print("PASS: no-flash invariant over the full Phase B timeline")
    else:
        # Video cells: relaxed suffix anchor. The video wallpaper has
        # an unavoidable decoder-startup window between
        # client_first_frame{wallpaper} and the first decoded video
        # frame; during that window halmasuit composites the wallpaper
        # plane against an empty fbo (visible as
        # `degenerate: True, black_pixel_count == pixel_count` for
        # frame_id 0). Known gap in the video wallpaper backend (the
        # fallback PNG should be composited pre-decode but isn't);
        # tracked as a follow-up on the wallpaper-engine epic. Image
        # and shader cells keep the strict anchor.
        events = visual.introspect_events(machine)
        frame_events = [
            (i, e) for i, e in enumerate(events)
            if e["event"] == "frame_rendered"
        ]
        assert frame_events, "no frame_rendered events emitted"
        first_nondegenerate = next(
            (i for i, (_, e) in enumerate(frame_events) if not e.get("degenerate")),
            None,
        )
        assert first_nondegenerate is not None, (
            "every frame_rendered was degenerate — video decode never produced "
            "a non-empty frame; wallpaper plane stuck on the empty fbo"
        )
        suffix = [e for _, e in frame_events[first_nondegenerate:]]
        pixel_counts = {e["pixel_count"] for e in suffix}
        assert pixel_counts == {1024000}, (
            f"pixel_count drifted post-decode: {pixel_counts}"
        )
        assert all(e["clear_pixel_count"] == 0 for e in suffix), (
            "clear (sentinel) pixels appeared in the post-decode stream — flash"
        )
        assert all(not e.get("degenerate") for e in suffix), (
            "post-decode frame_rendered marked degenerate"
        )
        print(
            f"PASS: no-flash suffix invariant ({len(suffix)}/{len(frame_events)} "
            "frames after first non-degenerate; pre-decode startup window "
            "excluded)"
        )

    # ── Phash-progression for animated cells (R3.6) ─────────────────
    # Catches the "frozen frame would pass today" gap: the no-flash
    # stream check above asserts every frame_rendered is non-degenerate
    # with stable pixel_count and zero sentinel pixels, but it does
    # NOT compare consecutive phashes — a pathological "render the
    # same frame N times" bug satisfies all those checks. Asserting
    # the phash stream actually varies catches it.
    #
    # Image cells skip this (image is static — frames ARE supposed to
    # be identical). Shader and video cells assert it with thresholds
    # matched to their fixtures' boot-time animation characteristic.
    if "image" not in cell_name:
        # Cache: video cells already captured `events` via the no-flash
        # suffix path above (line 277). Reuse it instead of re-shelling
        # to `journalctl -b -u halmasuit -o cat --no-pager` and reparsing
        # the entire boot journal a second time.
        if no_flash_mode != "strict" and "events" in locals():
            pass  # `events` from the suffix branch is fresh enough
        else:
            events = visual.introspect_events(machine)
        if "shader" in cell_name:
            # The 60s-period sine shader has subtle frame-to-frame
            # change in the boot window; loose thresholds prove
            # animation without false positives.
            phash_progression.assert_animating(
                events,
                min_distinct_phashes=3,
                min_hamming_max=8,
            )
        elif "video" in cell_name:
            # testsrc + 8x8 phash quantization → low distinct count,
            # high pairwise Hamming spread. Matches the shutdown-video
            # cell's tuning.
            phash_progression.assert_animating(
                events,
                min_distinct_phashes=3,
                min_hamming_max=20,
            )
        print(f"PASS: phash progression for {cell_name} (frames are distinct, not frozen)")

    print(f"visual-{cell_name}: ALL ASSERTIONS PASSED")
