# tests/halmasuit-multi-output.nix — regression gate for the per-CRTC
# multi-output scanout LOG LINE structure (Epic #15 subtask #19).
#
# Boots a NixOS VM with a single virtio-gpu-pci device. Asserts
# halmasuit's `build_drm_pipeline` gives each connected connector its
# own dedicated CRTC + DrmSurface + DrmCompositor (extended
# multi-output) and emits the structured tracing events:
#   * per output:  "DRM output: connector bound to dedicated CRTC"
#                  with fields connector / crtc / x / mode_w / mode_h /
#                  mode_hz
#   * summary:     "DRM scanout: per-CRTC multi-output initialized"
#                  with field `outputs`
#
# ── Scope limitation ───────────────────────────────────────────────
# Headless QEMU's virtio-gpu emulation only exposes ONE *Connected*
# DRM connector regardless of `max_outputs=N` — the extra connectors
# enumerate but report `Disconnected` because `-display none` means no
# monitors are physically attached at the QEMU layer. So this test
# CANNOT exercise the >1-connector path on its substrate. What it CAN
# do — and what makes it a useful regression gate — is:
#
#   1. Assert the per-output event fires with its structured-field
#      shape. A regression that breaks the logging (e.g. someone
#      refactors `build_drm_pipeline` and drops the `tracing::info!`)
#      flunks this test instantly.
#   2. Assert the summary reports `outputs=1` on a single-connector
#      substrate — i.e. the per-CRTC enumeration ran and produced
#      exactly one output (no regression to zero, no crash).
#
# The actual multi-connector behavior (NVIDIA driving DP-2 + DP-3 on
# TWO dedicated CRTCs as two extended wl_outputs) is validated by user
# reboot on real hardware — there is no documented way to make QEMU's
# virtio-gpu expose multiple *Connected* connectors in the NixOS
# headless test framework. The unit test
# `interface_prefix_pins_canonical_wayland_short_names` in drm.rs pins
# the connector short-name / primaryOutput-matching half of the logic.
#
# NOTE: the prior kernel-clone approach (one CRTC, multiple connectors)
# was removed — NVIDIA's open kernel module never sets `possible_clones`
# so the kernel rejects multi-connector commits; per-CRTC is the only
# viable path and the idiomatic smithay one (niri / cosmic-comp).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-multi-output";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        greeterCommand = "${pkgs.writeShellScript "halmasuit-test-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
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

      environment.systemPackages = [ halmasuit-vm-client ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'per-CRTC multi-output initialized'",
        timeout=30,
    )

    summary_line = machine.succeed(
        "journalctl -u halmasuit | grep 'per-CRTC multi-output initialized'"
    )
    output_line = machine.succeed(
        "journalctl -u halmasuit | grep 'connector bound to dedicated CRTC'"
    )

    # tracing-subscriber emits JSON-shape fields: `"field":value`. The
    # regex tolerates both that JSON shape AND the `field=value` shape
    # of structured-trace-text output, in case the subscriber config
    # changes. `(\d+)` captures the numeric value.
    def field(line, name):
        m = re.search(rf'"{name}":\s*(\d+)|{name}[=:]\s*(\d+)', line)
        assert m is not None, (
            f"field {name!r} missing in log line: {line!r}"
        )
        # Either group 1 (JSON path) or group 2 (text path) wins.
        return int(m.group(1) or m.group(2))

    # The per-CRTC enumeration ran and produced exactly one output on a
    # single-virtio-gpu headless substrate. The purpose of this test
    # isn't to validate multi-connector behavior (the substrate can't
    # expose two CONNECTED connectors); it's to ensure the per-CRTC
    # path EXECUTES and emits the structured events. If multi-connector
    # emulation becomes feasible later, bump the assertion.
    outputs = field(summary_line, "outputs")
    assert outputs == 1, f"expected outputs=1 on single-virtio-gpu, got {outputs}"

    # Each per-output line carries a real DRM mode (positive ints).
    mode_w = field(output_line, "mode_w")
    mode_h = field(output_line, "mode_h")
    mode_hz = field(output_line, "mode_hz")
    assert mode_w > 0 and mode_h > 0 and mode_hz > 0, (
        f"mode fields out of range: w={mode_w} h={mode_h} hz={mode_hz}"
    )

    print(
        f"halmasuit-multi-output: PASS "
        f"(outputs={outputs} mode={mode_w}x{mode_h}@{mode_hz})"
    )
  '';
}
