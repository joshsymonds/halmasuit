# Epic #71 R3.1/R3.2/R-honest.6 — diagnostic overlay chord + content gate.
#
# Proves the Linux SAK chord (Ctrl+Alt+Shift+Esc) opens the
# diagnostic overlay through halmasuit's REAL input layer (QEMU evdev
# → libinput → dispatch_libinput chord filter), that the overlay
# composes its mandated content from the live observability store
# (phase / broker / windows / journal), and that bare Esc closes it.
#
# Headless virtio-gpu paints black (CLAUDE.md rendering gotcha), so
# this asserts the journal MARKERS the open/close path logs — NOT
# pixels. The bitmap-rasterization correctness is unit-tested in
# R-honest.5 (console_font); the per-field live values are VM-tested
# in compositor1-dbus.nix. This test pins the input→toggle→compose
# wiring end-to-end:
#   - send_key Ctrl+Alt+Shift+Esc → "OVERLAY_OPEN: diagnostic content
#     composed" with content_len > 0 (real content, not an empty stub)
#   - send_key Esc → "OVERLAY_CLOSE: diagnostic overlay hidden"
#
# State-based polling throughout (wait_until_succeeds), never a bare
# time.sleep for a condition.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  testGreeter = pkgs.writeShellScript "halmasuit-overlay-greeter" ''
    exec ${pkgs.coreutils}/bin/sleep infinity
  '';
in
pkgs.testers.runNixOSTest {
  name = "diagnostic-overlay";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        greeterCommand  = "${testGreeter}";
        # Continuous shader wallpaper keeps the render loop live, so
        # the overlay toggle's immediate re-render has a healthy
        # pipeline (not strictly required — the chord forces a render
        # — but mirrors a real deployment).
        wallpaper = {
          type   = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
      };

      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
      };
      users.users.test.group = "test";
      users.groups.test.gid  = 1000;

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          # evdev keyboard libinput enumerates; machine.send_key
          # drives it (the chord must reach dispatch_libinput).
          "-device virtio-keyboard-pci"
        ];
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Render loop + input up.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'", timeout=30
    )

    # ── Open the overlay via the Linux SAK chord ──
    # QEMU sendkey accepts hyphen-joined chords; the keys arrive at
    # halmasuit's libinput, xkb resolves Ctrl+Alt+Shift held + Escape,
    # and the dispatch_libinput chord filter intercepts it. ONE chord
    # = one toggle (closed → open). The "OVERLAY_OPEN" marker is
    # logged on every open edge and never un-logged, so asserting it
    # appeared is robust to any later toggle. The seat keyboard exists
    # by scanout_active, so a single press registers.
    machine.send_key("ctrl-alt-shift-esc")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'OVERLAY_OPEN: diagnostic content composed'",
        timeout=30,
    )

    # content_len > 0 proves compose_overlay_text ran with REAL data
    # (phase + broker + window-count header lines), not an empty stub.
    journal = machine.succeed("journalctl -u halmasuit -o cat")
    lens = [int(m) for m in re.findall(r'content_len[\\":=]+(\d+)', journal)]
    assert lens, f"no content_len logged with OVERLAY_OPEN; journal tail:\n{journal[-1500:]}"
    assert max(lens) > 0, f"overlay content was empty (lens={lens}) — composer produced nothing"
    print(f"overlay content_len(s): {lens}")

    # ── Close the overlay with bare Esc ──
    # The single chord above left the overlay open; bare Esc (no
    # modifiers) hits the is_dismiss_key path and closes it.
    machine.send_key("esc")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'OVERLAY_CLOSE: diagnostic overlay hidden'",
        timeout=30,
    )

    print("=" * 70)
    print("PASS: Ctrl+Alt+Shift+Esc opened the diagnostic overlay through")
    print(f"      the real input path with real composed content "
          f"(len {max(lens)}); Esc closed it. Markers asserted (headless")
    print("      pixels are black; rasterization is unit-tested).")
    print("=" * 70)
  '';
}
