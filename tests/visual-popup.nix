# tests/visual-popup.nix — convergence epic R5: smoke gate for
# halmasuit's xdg_popup support (PopupManager wiring).
#
# Scope: this is a SMOKE gate, not a strict RED→GREEN signal. Smithay's
# `PopupSurface::send_configure` already consults the positioner state
# staged at `get_popup` time, so the configure geometry is non-zero
# even without halmasuit calling `PopupManager::track_popup`. R5's
# value is in the surrounding wiring — popup-tree registration,
# `popups.cleanup()` cadence, `find_popup()` availability — which is
# infrastructure that grab routing (R8) and reactive-popup
# reposition handling depend on. None of THAT is observable from a
# single-popup client.
#
# What this test does assert (no-regression guard):
#   - `halmasuit-popup-test-client` creates an xdg_toplevel +
#     xdg_popup with a deliberate positioner (200x100 size, anchor
#     rect 10,20,30,40).
#   - halmasuit emits the popup's configure event with non-zero
#     `width` and `height` (i.e., the positioner pipeline reaches
#     the wire — a future regression that broke it would surface
#     here).
#   - `assert_no_flash_stream` holds across the popup lifecycle.
#
# Strict popup-tree / cleanup / grab tests land with R8 when the
# `wl_seat` plumbing makes popup grabs observable.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-popup-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
in
pkgs.testers.runNixOSTest {
  name = "visual-popup";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, ... }:
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
        witnessImage    = ./fixtures/witness.png;
        greeterCommand  = "${halmasuit-popup-test-client}/bin/halmasuit-popup-test-client";
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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import os
    import re
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF POPUP_CONFIGURE", timeout=30
    )

    journal = machine.succeed("journalctl -u halmasuit --no-pager")
    line = next(
        (l for l in journal.splitlines() if "POPUP_CONFIGURE:" in l),
        None,
    )
    assert line is not None, (
        "POPUP_CONFIGURE not observed in halmasuit journal — "
        "the test client didn't reach the popup configure step."
    )
    print(f"observed: {line.strip()}")

    match = re.search(
        r"POPUP_CONFIGURE: x=(-?\d+) y=(-?\d+) w=(\d+) h=(\d+)",
        line,
    )
    assert match is not None, (
        f"POPUP_CONFIGURE line did not parse: {line!r}"
    )
    x, y, w, h = (int(v) for v in match.groups())
    print(f"parsed: x={x} y={y} w={w} h={h}")

    # The CONTRACT under test (R5 / xdg-shell positioner pipeline):
    # the popup configure MUST carry positioner-derived geometry, not
    # zeros. Pre-fix: w=0, h=0 (halmasuit forwards default geometry).
    # Post-fix: w>0, h>0 (smithay PopupManager + positioner-driven
    # geometry).
    assert w > 0 and h > 0, (
        f"R5 violated: popup configure carries zero-size geometry "
        f"({w}x{h}). halmasuit must wire the smithay PopupManager + "
        f"positioner-driven geometry pipeline so clients can map "
        f"popups."
    )

    visual.assert_no_flash_stream(machine)

    print("visual-popup: ALL ASSERTIONS PASSED")
  '';
}
