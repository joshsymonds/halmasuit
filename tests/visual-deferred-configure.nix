# tests/visual-deferred-configure.nix — convergence epic R4 + R6.
#
# R4 contract (xdg-shell.xml, `xdg_surface`): "The client must call
# wl_surface.commit ... before it will receive the initial configure
# event." The compositor sends the initial configure in response to
# the client's first wl_surface.commit on the surface — NOT eagerly
# at xdg_toplevel object creation. Smithay canonical pattern: send
# nothing protocol-visible from `new_toplevel`/`new_popup`; check the
# `initial_configure_sent` flag in the commit handler and send the
# initial configure once on first matching commit (smallvil pattern at
# `~/.cargo/git/checkouts/smithay-*/ff5fa7d/smallvil/src/handlers/xdg_shell.rs:152-189`).
#
# R6 contract (wayland.xml, `wl_surface::enter`): the server emits
# `wl_surface.enter` when a surface enters an output, so the client
# can pick buffer scale / transform / frame timing for that output.
# Pre-R6: halmasuit emitted enter for layer-shell surfaces (via
# `LayerMap::arrange`) but NOT for xdg-toplevels — multi-output-aware
# clients and HiDPI-aware toolkits (Qt 6, GTK 4) defaulted to wrong
# scale.
#
# The gate: `halmasuit-deferred-configure-test-client` is a raw-protocol
# wl_client (no SCTK Window abstraction — that auto-acks the
# configure, which would mask the contract). It drives:
#
#   PHASE 1 — after xdg_toplevel creation, before any wl_surface.commit:
#     emits `DEFERRED_CONFIGURE_PHASE1: configure_received=<bool>`
#     R4 CONTRACT: must be `false`.
#
#   PHASE 2 — after the first empty wl_surface.commit:
#     emits `DEFERRED_CONFIGURE_PHASE2: configure_received=<bool>`
#     R4 CONTRACT: must be `true`.
#
#   PHASE 3 — after ACK + buffered commit + roundtrip:
#     emits `SURFACE_ENTER_OBSERVED: <bool>`
#     R6 CONTRACT: must be `true` (halmasuit called `Output::enter`
#     on the toplevel and the client received `wl_surface.enter`).

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
    # The test client emits markers in order; PRESENTATION_FEEDBACK_OBSERVED
    # is the last one (R9 needs a VBlank to complete, so ~500ms sleep).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF PRESENTATION_FEEDBACK_OBSERVED",
        timeout=30,
    )

    journal = machine.succeed("journalctl -u halmasuit --no-pager")

    # PHASE 1 (R4): configure_received MUST be false (compositor MUST
    # NOT send initial configure before client's first commit).
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

    # PHASE 2 (R4): configure_received MUST be true (deferred initial
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

    # PHASE 3 (R6): SURFACE_ENTER_OBSERVED MUST be true.
    enter_line = next(
        (line for line in journal.splitlines()
         if "SURFACE_ENTER_OBSERVED:" in line),
        None,
    )
    assert enter_line is not None, (
        "SURFACE_ENTER_OBSERVED not observed in halmasuit journal."
    )
    print(f"PHASE 3: {enter_line.strip()}")
    assert "SURFACE_ENTER_OBSERVED: true" in enter_line, (
        f"R6 violated: halmasuit did NOT emit wl_surface.enter for "
        f"the xdg_toplevel. wayland.xml requires the compositor to "
        f"send `enter` so the client can pick buffer scale / "
        f"transform / frame timing per-output. Observed line: "
        f"{enter_line!r}"
    )

    # PHASE 4 (R10): DMABUF_GLOBAL_BOUND MUST be true. The test
    # client probed `zwp_linux_dmabuf_v1` from the registry; halmasuit
    # must advertise that global (with renderer-derived format
    # tranche) so Mesa-EGL clients can negotiate dmabuf buffers
    # instead of falling back to wl_shm.
    dmabuf_line = next(
        (line for line in journal.splitlines()
         if "DMABUF_GLOBAL_BOUND:" in line),
        None,
    )
    assert dmabuf_line is not None, (
        "DMABUF_GLOBAL_BOUND not observed in halmasuit journal."
    )
    print(f"PHASE 4: {dmabuf_line.strip()}")
    assert "DMABUF_GLOBAL_BOUND: true" in dmabuf_line, (
        f"R10 violated: halmasuit did NOT advertise "
        f"zwp_linux_dmabuf_v1. Mesa-EGL clients (Qt 6 / GTK 4) "
        f"will fall back to wl_shm — degraded perf and a known "
        f"wedge vector. Observed line: {dmabuf_line!r}"
    )

    # PHASE 5 (R9): wp_presentation global advertised AND a
    # `presented` / `discarded` event fires for surfaces that
    # requested feedback. Two assertions:
    presentation_bound_line = next(
        (line for line in journal.splitlines()
         if "PRESENTATION_GLOBAL_BOUND:" in line),
        None,
    )
    assert presentation_bound_line is not None, (
        "PRESENTATION_GLOBAL_BOUND not observed in halmasuit journal."
    )
    print(f"PHASE 5a: {presentation_bound_line.strip()}")
    assert "PRESENTATION_GLOBAL_BOUND: true" in presentation_bound_line, (
        f"R9 violated: halmasuit did NOT advertise wp_presentation. "
        f"Observed line: {presentation_bound_line!r}"
    )

    feedback_line = next(
        (line for line in journal.splitlines()
         if "PRESENTATION_FEEDBACK_OBSERVED:" in line),
        None,
    )
    assert feedback_line is not None, (
        "PRESENTATION_FEEDBACK_OBSERVED not observed in halmasuit journal."
    )
    print(f"PHASE 5b: {feedback_line.strip()}")
    assert "PRESENTATION_FEEDBACK_OBSERVED: true" in feedback_line, (
        f"R9 violated: halmasuit advertised wp_presentation but never "
        f"emitted `presented` (or `discarded`) for a surface that "
        f"requested feedback. Spec requires one of those per request. "
        f"Observed line: {feedback_line!r}"
    )

    # PHASE 6 (Phase B globals — advertise-and-delegate set): each
    # global must be bindable. The test client probed each from the
    # registry and emitted one journal marker per protocol.
    for marker in (
        "VIEWPORTER_GLOBAL_BOUND",
        "FRACTIONAL_SCALE_GLOBAL_BOUND",
        "SINGLE_PIXEL_BUFFER_GLOBAL_BOUND",
        "POINTER_GESTURES_GLOBAL_BOUND",
        "TABLET_MANAGER_GLOBAL_BOUND",
        "XDG_DECORATION_GLOBAL_BOUND",
    ):
        line = next(
            (l for l in journal.splitlines() if f"{marker}:" in l),
            None,
        )
        assert line is not None, (
            f"{marker} not observed in halmasuit journal."
        )
        print(f"PHASE 6 [{marker}]: {line.strip()}")
        assert f"{marker}: true" in line, (
            f"{marker} != true (Phase B global missing). "
            f"Observed line: {line!r}"
        )

    # No black/uncovered/degenerate frame across the run — the
    # witness threads continuously underneath. R4+R6 must NOT regress
    # the no-flash invariant.
    visual.assert_no_flash_stream(machine)

    print("visual-deferred-configure: ALL ASSERTIONS PASSED")
  '';
}
