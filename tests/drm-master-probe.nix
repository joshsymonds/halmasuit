# Phase 0 research probe: validate that a userspace process can hold DRM
# master from boot through multi-user.target on a stock NixOS system,
# without logind brokerage or contention.
#
# Substrate is intentionally minimal — no desktop stack — so the only
# thing exercising DRM is our probe.

{
  system,
  nixpkgs,
}:

let
  pkgs = import nixpkgs {
    inherit system;
  };

  drmMasterProbe = pkgs.rustPlatform.buildRustPackage {
    pname   = "drm-master-probe";
    version = "0.1.0";
    src     = ../.;
    cargoLock = {
      lockFile = ../Cargo.lock;
      allowBuiltinFetchGit = true;
    };
    cargoBuildFlags    = [ "-p" "drm-master-probe" ];
    doCheck = false;
  };
in
pkgs.testers.runNixOSTest {
  name = "drm-master-probe";

  # Interactive mode (`just test-vm-drive drm-master-probe`) swaps to
  # virtio-vga-gl so the painted red frame actually rasterizes in QEMU's
  # GTK window. Headless CI keeps virtio-gpu-pci — we assert via
  # debugfs + journal, not screenshot.
  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [ ./lib/test-user.nix ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };

      systemd.services.drm-master-probe = {
        description = "Phase 0 DRM master persistence probe (halmasuit v2)";
        after       = [ "local-fs.target" ];
        wantedBy    = [ "multi-user.target" ];
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${drmMasterProbe}/bin/drm-master-probe";
          Restart        = "no"; # failure IS the signal; do not loop
          StandardOutput = "journal";
          StandardError  = "journal";
        };
      };
    };

  testScript = ''
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("drm-master-probe.service")

    # Probe must reach both milestones in its journal.
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe | grep -qF 'SET_MASTER ok'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe | grep -qF 'SETCRTC ok'",
        timeout=30,
    )

    probe_pid = machine.succeed(
        "systemctl show -p MainPID --value drm-master-probe.service"
    ).strip()
    if not probe_pid or probe_pid == "0":
        raise AssertionError(
            f"drm-master-probe.service has no MainPID (output: {probe_pid!r})"
        )
    print(f"drm-master-probe PID = {probe_pid}")

    def assert_is_master(label):
        clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
        print(f"DRM clients at {label}:\n{clients}")
        for line in clients.splitlines():
            fields = line.split()
            # debugfs columns: command pid dev master a uid magic
            # comm is truncated to 15 chars; key on PID + master='y'.
            if len(fields) >= 4 and fields[1] == probe_pid and fields[3] == "y":
                print(f"PASS: probe PID {probe_pid} is DRM master at {label}")
                return
        raise AssertionError(
            f"probe PID {probe_pid} is not DRM master at {label}.\n"
            f"debugfs clients:\n{clients}"
        )

    assert_is_master("t=initial")
    time.sleep(5)
    assert_is_master("t+5s")

    # Heartbeat: at least 5 tick lines emitted across the elapsed time.
    tick_count = int(machine.succeed(
        "journalctl -u drm-master-probe | grep -cF ' tick t='"
    ).strip())
    if tick_count < 5:
        raise AssertionError(
            f"expected >=5 tick lines; got {tick_count}"
        )
    print(f"PASS: {tick_count} tick lines in journal")

    # logind must not have complained about DRM master.
    logind_errors = machine.succeed(
        "journalctl -u systemd-logind -p err..warning -b "
        "| grep -E 'master|TakeDevice|Failed to acquire' || true"
    ).strip()
    if logind_errors:
        raise AssertionError(
            f"systemd-logind reported errors:\n{logind_errors}"
        )
    print("PASS: no systemd-logind master conflicts")

    print("drm-master-probe: ALL ASSERTIONS PASSED")
  '';
}
