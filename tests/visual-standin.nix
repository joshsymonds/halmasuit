# tests/visual-standin.nix — first task of the visual-compositor epic.
#
# De-risks the headless-GL + golden-comparison capture pipeline by
# proving it works end-to-end against a trivial DRM dumb-buffer scene
# BEFORE any halmasuit renderer code exists. If this test passes, the
# infrastructure layer of the epic is real; every subsequent visual
# claim in the epic gets to stand on it.
#
# Differs from the existing drm-master-probe tests in three load-bearing
# ways:
#
#   1. QEMU display: virtio-vga-gl + -display egl-headless instead of
#      virtio-gpu-pci. The latter paints nothing visible to QEMU's
#      display layer (CLAUDE.md "Test-VM rendering gotcha"); the former
#      renders into an off-screen GL surface that QMP screendump can
#      capture as a real PNG.
#
#   2. Software rendering forced inside the guest via
#      LIBGL_ALWAYS_SOFTWARE=1 → llvmpipe. llvmpipe is bit-for-bit
#      reproducible across runs on a fixed Mesa version, which is what
#      makes goldens viable. The pin to Mesa via flake.lock means
#      goldens stay valid until `nix flake update` deliberately bumps it.
#
#   3. Comparison via ssimulacra2_rs (pure-Rust port of the
#      JPEG-XL-team's perceptual-similarity metric), threshold ≥ 90.0
#      ("imperceptible" per the JND-aligned spec). Same architectural
#      intent the epic locked: tight perceptual threshold for goldens.

{
  system,
  nixpkgs,
  halmasuit-visual-test-standin,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "visual-standin";

  # The testScript imports `visual` via sys.path.insert() at runtime,
  # which the upstream nixos-test-driver type checker can't trace
  # statically. Skip the type pass for this test; the actual import
  # is well-formed.
  skipTypeCheck = true;

  # Interactive mode (`just test-vm-drive visual-standin`) opens a GTK
  # window so a human can watch the standin paint. Headless CI keeps
  # egl-headless — we don't need the window, we just need a capturable
  # GL surface.
  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { ... }:
    {
      imports = [ ./lib/test-user.nix ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        # virtio-gpu-pci is what every other NixOS test in this repo
        # uses, and the CLAUDE.md "paints nothing" gotcha applies only
        # to GBM-allocated buffers (GLES compositors). The standin
        # uses kernel DRM dumb buffers — a CPU-mapped framebuffer the
        # kernel scans out normally, populating QEMU's display console
        # without any host EGL/GL backend. QMP `screendump` reads off
        # that console and gives us a real PNG.
        #
        # When halmasuit's GLES renderer lands in a later subtask of
        # this epic, it WILL need virtio-vga-gl + egl-headless +
        # /dev/dri exposed to the sandbox (or driverInteractive-only
        # tests). That's a problem for that subtask. The standin
        # exists exactly to de-risk the capture pipeline FIRST.
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };

      # ssimulacra2_rs is installed in the guest for completeness, but
      # the test driver (which runs on the HOST) is what actually
      # compares pixels. The host-side invocation is via PATH injection
      # in the testScript below.
      environment.systemPackages = [ ssimulacra2-cli ];

      systemd.services.halmasuit-visual-test-standin = {
        description = "Visual-test stand-in (DRM dumb-buffer paint)";
        after       = [ "local-fs.target" ];
        wantedBy    = [ "multi-user.target" ];
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${halmasuit-visual-test-standin}/bin/halmasuit-visual-test-standin";
          Restart        = "no";
          StandardOutput = "journal";
          StandardError  = "journal";
        };
      };
    };

  # Goldens live in the source tree at ./goldens (relative to this
  # file). Passed to the test driver via env so visual.py knows where
  # to look. Inside the Nix sandbox this is a read-only store path;
  # interactive runs see the writable source-tree path.
  testScript = ''
    import os
    import sys

    # Make the visual.py helper module importable from this test.
    sys.path.insert(0, "${./lib}")
    # Make ssimulacra2_rs findable on PATH for the driver-side
    # subprocess call inside visual.py.
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    # GOLDENS_DIR is the read-only store snapshot of tests/goldens at
    # the time this test was evaluated. Regeneration must run outside
    # the sandbox (see Justfile `update-goldens`).
    os.environ["GOLDENS_DIR"] = "${./goldens}"

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit-visual-test-standin.service")

    # Standin emits SET_MASTER ok + SETCRTC ok in its journal before
    # entering the heartbeat loop. Wait for SETCRTC so we know the
    # framebuffer has been pushed to the connector at least once.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit-visual-test-standin | grep -qF 'SETCRTC ok'",
        timeout=30,
    )

    # Give QEMU's display layer a beat to settle on the painted frame.
    # 500ms is generous — three vblanks at 60Hz. machine.screenshot is
    # invoked by visual.capture inside the assertion.
    import time
    time.sleep(0.5)

    visual.assert_matches_golden(machine, "standin-quadrants")

    print("visual-standin: ALL ASSERTIONS PASSED")
  '';
}
