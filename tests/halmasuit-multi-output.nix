# tests/halmasuit-multi-output.nix — regression gate for the
# multi-connector kernel-clone scanout LOG LINE structure
# (Epic #15 subtask #19 + #20).
#
# Boots a NixOS VM with a single virtio-gpu-pci device. Asserts
# halmasuit's `build_drm_pipeline` emits the structured tracing event
# "DRM scanout: binding connectors to single CRTC for kernel-clone"
# with the expected JSON-shape fields (`total_connectors`,
# `cloned_connectors`, `mode_w`, `mode_h`, `mode_hz`).
#
# ── Scope limitation ───────────────────────────────────────────────
# Headless QEMU's virtio-gpu emulation only exposes ONE *Connected*
# DRM connector regardless of `max_outputs=N` — the extra connectors
# enumerate but report `Disconnected` because `-display none` means
# no monitors are physically attached at the QEMU layer. So this test
# CANNOT exercise the multi-connector clone path on its substrate.
# What it CAN do — and what makes it a useful regression gate — is:
#
#   1. Assert the tracing event fires with its structured-field shape.
#      A regression that breaks the logging (e.g. someone refactors
#      `build_drm_pipeline` and drops the `tracing::info!`) flunks
#      this test instantly.
#   2. Assert the single-connector path's counts are 1/1 (one
#      connected, one cloned). A regression that hardcodes the
#      counts to 0/0 or doesn't enter the cloning block flunks here.
#
# The actual multi-connector clone behavior (NVIDIA driving DP-2 +
# DP-3 from one CRTC) is validated by user reboot on real hardware
# — there is no documented way to make QEMU's virtio-gpu expose
# multiple *Connected* connectors in the NixOS headless test
# framework. The unit test
# `interface_prefix_pins_canonical_wayland_short_names` in drm.rs
# pins the primaryOutput-matching half of the logic.

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
        "journalctl -u halmasuit | grep -qF 'DRM scanout: binding connectors'",
        timeout=30,
    )

    scanout_line = machine.succeed(
        "journalctl -u halmasuit | grep 'DRM scanout: binding connectors'"
    )

    # tracing-subscriber emits JSON-shape fields: `"field":value`. The
    # regex tolerates both that JSON shape AND the `field=value` shape
    # of structured-trace-text output, in case the subscriber config
    # changes. `(\d+)` captures the numeric value.
    def field(name):
        m = re.search(rf'"{name}":\s*(\d+)|{name}[=:]\s*(\d+)', scanout_line)
        assert m is not None, (
            f"field {name!r} missing in scanout log line: {scanout_line!r}"
        )
        # Either group 1 (JSON path) or group 2 (text path) wins.
        return int(m.group(1) or m.group(2))

    total = field("total_connectors")
    cloned = field("cloned_connectors")
    mode_w = field("mode_w")
    mode_h = field("mode_h")
    mode_hz = field("mode_hz")

    # On a single-virtio-gpu headless substrate we expect 1/1. The
    # purpose of this test isn't to validate multi-connector behavior
    # (the substrate can't expose two CONNECTED connectors); it's to
    # ensure the cloning code path EXECUTES and emits the structured
    # event with the expected fields. If multi-connector emulation
    # becomes feasible later, bump the assertion.
    assert total == 1, f"expected total_connectors=1 on single-virtio-gpu, got {total}"
    assert cloned == 1, f"expected cloned_connectors=1 on single-virtio-gpu, got {cloned}"

    # Mode fields must be present and sensible (positive ints).
    assert mode_w > 0 and mode_h > 0 and mode_hz > 0, (
        f"mode fields out of range: w={mode_w} h={mode_h} hz={mode_hz}"
    )

    print(
        f"halmasuit-multi-output: PASS "
        f"(total={total} cloned={cloned} mode={mode_w}x{mode_h}@{mode_hz})"
    )
  '';
}
