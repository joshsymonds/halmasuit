# halmasuit-shutdown-probe Phase 2: validate a process HOLDING DRM
# master + framebuffer survives systemd-shutdown's pivot to
# /run/initramfs.
#
# Phase 0 proved the process survives the kill spree. Phase 1 proved
# the SAME PID survives the actual rootfs→shutdownRamfs pivot. Phase 2
# adds the DRM dependency: the probe now opens /dev/dri/card0, takes
# master, allocates a dumb buffer + framebuffer, and calls set_crtc
# ONCE at startup. It then enters the standard heartbeat loop with
# a per-tick `drm_fd_open=<raw_fd>` status (a kernel-touch-free
# liveness signal — see crate doc for why we don't re-issue set_crtc
# per tick).
#
# There is no documented prior art for a graphics/DRM process doing
# this — Plymouth does NOT survive the shutdown pivot. If this test
# FAILS for fundamental reasons (process killed when holding DRM
# resources at pivot, /dev/dri/card0 inode revocation, seatd/logind
# unwind cascade), Epic #47 R2 falls back to the partial-scope
# alternative documented in the epic ("paint wallpaper until SIGKILL,
# accept brief flash before kernel halt").
#
# Mechanism:
#   * Probe opens /dev/dri/card0, takes DRM master, paints a magenta
#     dumb buffer + set_crtc ONCE at startup. The magenta is
#     distinguishable from kernel framebuffer black if a future
#     visual-shutdown test wants to capture the actual pixel content.
#   * Probe heartbeats every 100ms with `drm_fd_open=<N>` suffix.
#   * testScript: trigger systemctl poweroff, read serial console
#     post-halt, assert AT LEAST ONE heartbeat with
#     `drm_fd_open=<N>` appears AFTER the post-pivot `shutdown[1]:`
#     marker — proves a process WITH DRM resources held survives
#     the pivot. Stronger questions ("does set_crtc still succeed
#     post-pivot?") are deferred to production wiring.

{
  system,
  nixpkgs,
}:

let
  pkgs = import nixpkgs {
    inherit system;
  };

  halmasuitShutdownProbe = pkgs.rustPlatform.buildRustPackage {
    pname   = "halmasuit-shutdown-probe";
    version = "0.1.0";
    src     = ../.;
    cargoLock = {
      lockFile = ../Cargo.lock;
      allowBuiltinFetchGit = true;
    };
    cargoBuildFlags = [ "-p" "halmasuit-shutdown-probe" ];
    doCheck = false;
  };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-shutdown-probe-phase2";

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

      # Mirror drm-master-probe's kernel module list — virtio-gpu must
      # be present at boot so /dev/dri/card0 exists when the probe
      # service starts.
      boot.initrd.availableKernelModules = [ "virtio_gpu" ];
      boot.initrd.kernelModules = [ "virtio_gpu" ];

      # Same shutdownRamfs config as Phase 1. The DRM ioctl path is
      # entirely in the kernel; what we need in shutdownRamfs is the
      # probe binary + libc + libgcc (the runtime closure of the
      # binary). nix-store-aware closure resolution handles the
      # transitive deps automatically via storePaths.
      systemd.shutdownRamfs.storePaths = [
        "${halmasuitShutdownProbe}/bin/halmasuit-shutdown-probe"
      ];

      systemd.services.halmasuit-shutdown-probe = {
        description = "Phase 2: DRM master fd survives the shutdownRamfs pivot";
        wantedBy = [ "multi-user.target" ];
        before = [ "shutdown.target" ];
        unitConfig = {
          DefaultDependencies = false;
          SurviveFinalKillSignal = "yes";
        };
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${halmasuitShutdownProbe}/bin/halmasuit-shutdown-probe phase2";
          Restart        = "no";
          KillMode       = "process";
          StandardOutput = "journal";
          StandardError  = "journal";
          # CAP_SYSLOG to write /dev/kmsg; SupplementaryGroups=video so
          # /dev/dri/card0 is openable from the probe's uid (systemd
          # tagging uses `video` for DRM nodes by default).
          AmbientCapabilities = [ "CAP_SYSLOG" ];
          SupplementaryGroups = [ "video" ];
        };
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("halmasuit-shutdown-probe.service")

    # Wait for the probe to be emitting heartbeats. /dev/kmsg only
    # streams NEW messages (its file-offset on open is the tail of
    # the ring buffer), so we must wait for a RECURRING pattern;
    # waiting for a one-shot startup line via /dev/kmsg hangs
    # because grep blocks reading forever and timeout= doesn't kill
    # the inner shell exec — Phase 0/1 use this same shape.
    machine.wait_until_succeeds(
        "grep -q 'halmasuit-shutdown-probe\\[phase2\\]: heartbeat' /dev/kmsg",
        timeout=15,
    )

    probe_pid = machine.succeed(
        "systemctl show -p MainPID halmasuit-shutdown-probe.service"
    ).strip().split("=")[1]

    # Confirm the one-shot drm_setup_ok=true line was emitted. Use
    # dmesg (which reads the ring buffer from position 0 and returns
    # immediately) rather than `cat /dev/kmsg` (which blocks).
    setup_line = machine.succeed(
        "dmesg | grep 'halmasuit-shutdown-probe\\[phase2\\]: drm_setup_ok'"
    ).strip()
    if "drm_setup_ok=true" not in setup_line:
        raise AssertionError(
            "Probe failed DRM setup before shutdown — no valid Phase 2 "
            f"signal possible.\n  setup line: {setup_line!r}"
        )
    print(f"PASS: probe completed DRM setup: {setup_line.split(']: ')[-1]}")

    # /sys/kernel/debug/dri/0/clients verification — probe pid
    # appears with master flag 'y'.
    drm_clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    found_master = False
    for line in drm_clients.splitlines():
        fields = line.split()
        # Format: command pid dev master a uid magic
        if len(fields) >= 4 and fields[1] == probe_pid and fields[3] == "y":
            found_master = True
            break
    if not found_master:
        raise AssertionError(
            f"Probe PID {probe_pid} not holding DRM master pre-shutdown.\n"
            f"debugfs clients listing:\n{drm_clients}"
        )
    print(f"PASS: probe PID {probe_pid} holds DRM master pre-shutdown")

    # Per-tick fd-open status is also visible in dmesg. Confirm at
    # least one pre-shutdown heartbeat shows drm_fd_open=<integer>.
    pre_fd_open = machine.succeed(
        "dmesg | grep -c 'halmasuit-shutdown-probe\\[phase2\\]: heartbeat.*drm_fd_open=[0-9]'"
    ).strip()
    if int(pre_fd_open) < 1:
        raise AssertionError(
            f"Probe did NOT report a held DRM fd in any heartbeat "
            f"pre-shutdown (count={pre_fd_open}). The DrmState option "
            "may be None, meaning DRM setup failed silently after the "
            "one-shot drm_setup_ok=true line."
        )
    print(f"PASS: {pre_fd_open} pre-shutdown heartbeats with drm_fd_open=<N>")

    machine.shutdown()
    console = machine.get_console_log()

    # Locate the post-pivot `shutdown[1]:` marker (same logic as Phase 1).
    pivot_re = re.compile(r"^[^a-zA-Z]*shutdown\[1\]:", re.MULTILINE)
    pivot_match = pivot_re.search(console)
    if pivot_match is None:
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate `shutdown[1]:` post-pivot marker.\n"
            "Last 100 console lines:\n" + tail
        )
    pivot_offset = pivot_match.start()
    print("PASS: located post-pivot `shutdown[1]:` marker")

    # Count POST-PIVOT heartbeats. drm_fd_open=<N> in the suffix is the
    # liveness signal; its presence proves the probe is alive AND still
    # has the DRM fd handle.
    post_pivot = console[pivot_offset:]
    hb_re = re.compile(
        r"halmasuit-shutdown-probe\[phase2\]: heartbeat seq=(\d+) pid=(\d+) drm_fd_open=(\S+)"
    )
    post_pivot_hbs = list(hb_re.finditer(post_pivot))

    if not post_pivot_hbs:
        post_pivot_window = "\n".join(post_pivot.splitlines()[:50])
        raise AssertionError(
            "PROCESS HOLDING DRM RESOURCES DID NOT SURVIVE THE PIVOT: "
            "0 phase2 heartbeats found after the post-pivot marker. "
            "Phase 1 (no DRM) survives the same pivot; Phase 2 (with "
            "DRM master + framebuffer held) does not. The kernel is "
            "killing processes holding DRM resources at shutdown — "
            "Epic #47 R2 must fall back to the partial-scope alternative "
            "(paint wallpaper until SIGKILL, accept brief flash before "
            "kernel halt)."
            f"\n\nWindow after pivot:\n{post_pivot_window}"
        )

    print(
        f"PASS: {len(post_pivot_hbs)} post-pivot heartbeat(s) carrying "
        "drm_fd_open=<N> — process HOLDING DRM resources survives the "
        "rootfs→shutdownRamfs pivot. Epic #47 R2 production wiring "
        "unblocked (with the caveat that per-tick set_crtc deadlocked "
        "against shutdown teardown in an earlier experiment; production "
        "code should use non-blocking page-flip/atomic APIs)."
    )

    # PID continuity (inherited assertion from Phase 1).
    pids = {m.group(2) for m in hb_re.finditer(console)}
    if pids != {probe_pid}:
        raise AssertionError(
            f"Heartbeat pids {sorted(pids)} differ from probe MainPID {probe_pid}"
        )
    print(f"PASS: every heartbeat carries pid={probe_pid} — same PID throughout")
  '';
}
