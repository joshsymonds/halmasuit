# tests/visual-phase-b-side-image.nix — Phase B golden-boot,
# side-volume LUKS × image wallpaper. Epic #35 cell (side, image).
#
# The composed end-to-end claim: cold boot into initramfs halmasuit
# (with image wallpaper composited from frame 0) + halmasuit-luks →
# LUKS side-volume unlocks via the production password-agent wire →
# switch_root → halmasuit chroots into rootfs via broker root-fd
# handoff → DankGreeter (DMS Quickshell) spawns as the configured
# greeter → real keyboard input (machine.send_chars) drives alice's
# PAM auth → broker forks-then-drops niri as the session → niri
# nests as a wayland client of halmasuit and maps its toplevel.
#
# Visual coverage layers:
#   1. assert_no_flash_stream over every frame_rendered event from
#      initramfs through session_opened (zero clear/degenerate frames,
#      constant pixel_count, wallpaper plane composited from frame 0).
#   2. SSIMULACRA2 goldens at greeter scene + session scene
#      (phase-b-side-image-{greeter,session}.png).
#   3. Lifecycle event cross-assertions (started → phase_entered
#      sequence → greeter_spawned → foreground_changed → session_opened
#      → foreground_changed{session}). PID continuous across the swap.
#
# This is the first cell of the matrix; the other five follow the
# same shape with parametric wallpaper variant and (for encrypted-
# root cells) a different boot-specialisation dance.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit-debug,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    # niri-flake's niri-unstable may pull unfree deps transitively
    # (same rationale as visual-niri-session.nix).
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-side-image";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine = {
    imports = [
      ../nix/module.nix
      (import ./lib/phase-b-golden.nix {
        wallpaper = {
          type = "image";
          source = ./fixtures/wallpaper.png;
        };
        lukshape = "side-volume";
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client nix-config;
        wallpaperStorePaths = [ ./fixtures/wallpaper.png ];
      })
    ];
  };

  testScript = ''
    import os
    import sys
    import time

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    # ── First boot: /dev/vdb not yet LUKS. ───────────────────────────
    # The cryptsetup unit for test-luks-data fails (no LUKS header),
    # but multi-user.target still reaches because the unit has no
    # hard dependents. We luksFormat /dev/vdb at runtime and reboot.
    machine.start()
    machine.wait_for_unit("multi-user.target")
    print("PASS: first boot reached multi-user.target")

    # luksFormat /dev/vdb with the canonical passphrase the
    # halmasuit-luks responder will return. --iter-time 1 keeps the
    # keyslot KDF cheap so the VM test stays fast.
    machine.succeed(
        "printf 'luks-test-unlock-secret' | "
        "cryptsetup luksFormat -q --iter-time 1 /dev/vdb -"
    )
    print("PASS: /dev/vdb formatted with canonical passphrase")
    machine.succeed("sync")

    # ── Crash + reboot. Same config; this time /dev/vdb IS LUKS. ─────
    machine.crash()
    machine.start()
    machine.wait_for_unit("multi-user.target")
    print("PASS: second boot reached multi-user.target")

    # Diagnostics — figure out why cryptsetup didn't unlock /dev/vdb.
    print("=== /etc/crypttab (rootfs) ===")
    print(machine.execute("cat /etc/crypttab 2>&1 || echo MISSING")[1])
    print("=== systemctl list-units '*crypt*' --all ===")
    print(machine.execute("systemctl list-units '*crypt*' --all --no-pager 2>&1")[1])
    print("=== systemctl list-unit-files '*crypt*' ===")
    print(machine.execute("systemctl list-unit-files '*crypt*' --no-pager 2>&1")[1])
    print("=== journalctl initrd cryptsetup events ===")
    print(machine.execute(
        "journalctl -b --output=cat --no-pager | "
        "grep -iE 'cryptsetup|crypttab|test-luks-data|luks' | head -50"
    )[1])
    print("=== cryptsetup isLuks /dev/vdb ===")
    print(machine.execute("cryptsetup isLuks /dev/vdb && echo yes || echo no")[1])

    # ── LUKS unlock assertion ────────────────────────────────────────
    mapper = machine.succeed("ls /dev/mapper").strip()
    assert "test-luks-data" in mapper.split(), (
        f"/dev/mapper/test-luks-data not present after the agent-driven "
        f"unlock.\n/dev/mapper contents: {mapper}\n"
        + machine.execute(
            "journalctl -b -u halmasuit-luks --output=cat --no-pager"
        )[1]
    )
    print("PASS: /dev/mapper/test-luks-data unlocked via initramfs agent path")

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
    # Wait for the post-pivot setup to complete (greeter_spawned is
    # the last event emitted by run_post_pivot_setup).
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"greeter_spawned\\\"'",
        timeout=60,
    )
    print("PASS: greeter spawned post-pivot")

    # DMS Quickshell is the greeter. Wait for its layer surface to
    # show up (Quickshell's wlr-layer-shell binding) so we know
    # keyboard input will land on a focused surface, not the wallpaper.
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
    assert "greeter" in fg_events(), (
        f"expected greeter foreground; got {fg_events()}"
    )
    print("PASS: foreground=greeter; DankGreeter layer surface up")

    # Greeter-scene golden BEFORE typing alice's credentials.
    time.sleep(2)
    visual.assert_matches_golden(machine, "phase-b-side-image-greeter")
    print("PASS: greeter-scene golden")

    # ── R13(b) real keyboard arc through DankGreeter ────────────────
    halmasuit_pid = machine.execute(
        "pgrep -f /halmasuit$ | head -1"
    )[1].strip()
    assert halmasuit_pid, "couldn't find halmasuit PID"
    print(f"halmasuit PID = {halmasuit_pid}")

    # DMS QML uses ONE TextField that toggles between username and
    # password mode on Enter. The username→password transition is
    # client-side state inside Quickshell — no journal-visible marker.
    # Same 1-second settle as visual-dankgreeter-auth.nix.
    machine.send_chars("alice")
    machine.send_key("ret")
    time.sleep(1)
    machine.send_chars("testpassword")
    machine.send_key("ret")

    # Real pam_unix → session_opened. halmasuit emits the event as a
    # JSON-escaped entry; grep on the bare token.
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager | grep -qF session_opened",
        timeout=120,
    )
    print("PASS: session_opened (real pam_unix through broker)")

    # Real niri came up as the broker-launched session.
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F 'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )
    assert fg_events()[:2] == ["greeter", "session"], (
        f"foreground ordering wrong: {fg_events()}"
    )
    print("PASS: niri mapped fullscreen; foreground=session")

    # halmasuit PID continuous across the swap (login-flash invariant).
    pid_now = machine.execute("pgrep -f /halmasuit$ | head -1")[1].strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: "
        f"{halmasuit_pid} -> {pid_now}"
    )
    print(f"PASS: halmasuit pid {halmasuit_pid} continuous")

    # Session-scene golden AFTER niri maps.
    time.sleep(2)
    visual.assert_matches_golden(machine, "phase-b-side-image-session")
    print("PASS: session-scene golden")

    # ── No-flash invariant over the whole kernel-handoff-to-session ─
    visual.assert_no_flash_stream(machine)
    print("PASS: no-flash invariant over the full Phase B timeline")

    print("visual-phase-b-side-image: ALL ASSERTIONS PASSED")
  '';
}
