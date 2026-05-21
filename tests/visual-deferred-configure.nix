# tests/visual-deferred-configure.nix — convergence epic R4: regression
# test for deferred initial xdg_surface.configure.
#
# xdg-shell spec (xdg-shell.xml, `xdg_surface`): "The client must call
# wl_surface.commit ... before it will receive the initial configure
# event." The compositor sends the initial configure in response to
# the client's first wl_surface.commit on the surface — NOT eagerly
# at xdg_toplevel object creation. Smithay canonical pattern: send
# nothing protocol-visible from `new_toplevel`/`new_popup`; check the
# `initial_configure_sent` flag in the commit handler and send the
# initial configure once on first matching commit (smallvil pattern at
# `~/.cargo/git/checkouts/smithay-*/ff5fa7d/smallvil/src/handlers/xdg_shell.rs:152-189`).
#
# The gate: `halmasuit-deferred-configure-test-client` is a raw-protocol
# wl_client (no SCTK Window abstraction — that auto-acks the
# configure, which would mask the contract). It drives two phase
# observations to stderr:
#
#   PHASE 1 — after xdg_toplevel creation, before any wl_surface.commit:
#     emits `DEFERRED_CONFIGURE_PHASE1: configure_received=<bool>`
#     CONTRACT: must be `false` (a conformant compositor has not
#     yet sent the initial configure — it's waiting for the client's
#     first commit per spec).
#
#   PHASE 2 — after the first empty wl_surface.commit:
#     emits `DEFERRED_CONFIGURE_PHASE2: configure_received=<bool>`
#     CONTRACT: must be `true` (the deferred initial configure
#     fires now, in response to the client's first commit).
#
# The test driver greps the test client's stderr (via the journal of
# halmasuit, which captures its child's stderr) and asserts both
# contracts hold. No `frame_audit`, no goldens — protocol-conformance
# only.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-deferred-configure-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
in
pkgs.testers.runNixOSTest {
  name = "visual-deferred-configure";

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
        package         = halmasuit; # halmasuit-debug via flake (frame_audit events)
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        witnessImage    = ./fixtures/witness.png;
        greeterCommand  = "${halmasuit-deferred-configure-test-client}/bin/halmasuit-deferred-configure-test-client";
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
    import sys

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    # halmasuit is up and has spawned the test client as its greeter.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    # The test client emits both phase markers very quickly (no
    # network IO, single roundtrip each). Wait for the PHASE 2
    # marker — the slower one — which signals the full observation
    # is done.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF DEFERRED_CONFIGURE_PHASE2",
        timeout=30,
    )

    journal = machine.succeed("journalctl -u halmasuit --no-pager")

    # PHASE 1: configure_received MUST be false (compositor MUST NOT
    # send initial configure before client's first commit).
    phase1_line = next(
        (line for line in journal.splitlines()
         if "DEFERRED_CONFIGURE_PHASE1:" in line),
        None,
    )
    assert phase1_line is not None, (
        "DEFERRED_CONFIGURE_PHASE1 not observed in halmasuit journal — "
        "test client didn't reach the phase 1 emission. Full journal "
        "tail above."
    )
    print(f"PHASE 1: {phase1_line.strip()}")
    assert "configure_received=false" in phase1_line, (
        f"R4 violated: compositor sent initial xdg_surface.configure "
        f"BEFORE the client's first wl_surface.commit. xdg-shell spec "
        f"requires the configure to be sent in response to the first "
        f"commit. Observed line: {phase1_line!r}"
    )

    # PHASE 2: configure_received MUST be true (deferred initial
    # configure fires in response to the first commit).
    phase2_line = next(
        (line for line in journal.splitlines()
         if "DEFERRED_CONFIGURE_PHASE2:" in line),
        None,
    )
    assert phase2_line is not None, (
        "DEFERRED_CONFIGURE_PHASE2 not observed in halmasuit journal."
    )
    print(f"PHASE 2: {phase2_line.strip()}")
    assert "configure_received=true" in phase2_line, (
        f"R4 broken: compositor did NOT send the deferred initial "
        f"configure in response to the client's first commit. Clients "
        f"will hang. Observed line: {phase2_line!r}"
    )

    # No black/uncovered/degenerate frame across the run — the
    # witness threads continuously underneath. R4 must NOT regress
    # the no-flash invariant.
    visual.assert_no_flash_stream(machine)

    print("visual-deferred-configure: ALL ASSERTIONS PASSED")
  '';
}
