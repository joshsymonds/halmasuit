# Phase 1 research probe: validate that a userspace process started in
# initramfs can survive switch_root via systemd's @argv[0] convention,
# drop privileges to a non-root UID via setresuid, and continue holding
# DRM master continuously across the entire transition.
#
# Sibling to tests/drm-master-probe.nix (Phase 0). Builds on:
#   - boot.initrd.systemd.enable = true  (systemd-in-initramfs)
#   - boot.initrd.systemd.storePaths     (ship probe binary into initrd)
#   - boot.initrd.systemd.services       (register the probe as an
#                                          initramfs service)
#
# The probe's argv[0] mutation makes it survive systemd's switch_root
# killing spree. From rootfs systemd's perspective the probe is then an
# orphan (not a registered service), so the test driver locates it via
# journal + /proc rather than systemctl.

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
  name = "drm-master-probe-phase1";

  # Interactive mode swaps to virtio-vga-gl so the painted red frame
  # rasterizes in QEMU's GTK window — humans can visually confirm the
  # red appears from very early in boot and persists through to the
  # post-switchroot phase.
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

      # systemd-in-initramfs is required for boot.initrd.systemd.services.
      boot.initrd.systemd.enable = true;

      # /dev/dri/card0 only exists once the DRM driver loads. NixOS's
      # default initramfs ships virtio-blk-style storage modules but not
      # virtio_gpu (it's not needed for booting). availableKernelModules
      # COPIES the module into the initramfs; kernelModules force-LOADS it
      # via systemd-modules-load.service.
      boot.initrd.availableKernelModules = [ "virtio_gpu" ];
      boot.initrd.kernelModules = [ "virtio_gpu" ];

      # Ship the probe binary into the initramfs.
      boot.initrd.systemd.storePaths = [
        "${drmMasterProbe}/bin/drm-master-probe"
      ];

      # Initramfs service. Survival across switch_root requires:
      #   - DefaultDependencies = false (avoid the default stop-on-shutdown chain)
      #   - IgnoreOnIsolate = true (don't stop when isolating switch-root target)
      #   - argv[0][0] = '@' inside the probe itself (excludes the process
      #     from systemd's post-pivot_root killall — see ROOT_STORAGE_DAEMONS)
      # With the first two set, the unit is never stopped by systemd, so
      # KillMode doesn't fire — no need to set it (and KillMode=none is
      # deprecated as of recent systemd).
      # Diagnostic pre-service: dumps DRM-related state to journal so we
      # can see whether virtio_gpu loaded and /dev/dri/card0 exists at the
      # moment the probe is about to start.
      boot.initrd.systemd.services.drm-diag = {
        description = "Initramfs DRM diagnostic dump";
        wantedBy = [ "initrd.target" ];
        after    = [ "systemd-modules-load.service" "systemd-udev-settle.service" ];
        before   = [ "drm-master-probe.service" "initrd-switch-root.service" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type           = "oneshot";
          ExecStart      = "${pkgs.bash}/bin/bash -c 'echo DRM_DIAG_SYS:; ls -la /sys/class/drm/ 2>&1 | head -20; echo DRM_DIAG_DEV:; ls -la /dev/dri/ 2>&1; echo DRM_DIAG_LSMOD:; cat /proc/modules 2>&1 | grep -E \"drm|virtio\" | head -20'";
          StandardOutput = "journal";
          StandardError  = "journal";
        };
      };

      boot.initrd.systemd.services.drm-master-probe = {
        description = "Phase 1 DRM master persistence probe (halmasuit v2)";
        wantedBy = [ "initrd.target" ];
        after    = [ "systemd-modules-load.service" "systemd-udev-settle.service" "drm-diag.service" ];
        before   = [ "initrd-switch-root.service" ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate     = true;
        };
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${drmMasterProbe}/bin/drm-master-probe";
          Restart        = "no";   # failure IS the signal
          StandardOutput = "journal";
          StandardError  = "journal";
        };
      };
    };

  testScript = ''
    import re
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Phase 1's probe redirects its stderr to /run/drm-master-probe.log
    # (so writes don't depend on journald's stderr pipe surviving
    # switch_root), and writes any signal that kills it to
    # /run/drm-master-probe-events.log via async-signal-safe handlers.
    # Give the probe time to die if it's going to.
    time.sleep(10)

    print("=== /run/drm-master-probe.log ===")
    print(machine.succeed("cat /run/drm-master-probe.log"))
    print("=== /run/drm-master-probe-events.log ===")
    print(machine.execute("cat /run/drm-master-probe-events.log 2>&1")[1])
    print("=== systemctl status drm-master-probe.service ===")
    print(machine.execute(
        "systemctl status drm-master-probe.service 2>&1 || true"
    )[1])
    print("=== journalctl initramfs DRM_DIAG ===")
    print(machine.execute(
        "journalctl -b | grep DRM_DIAG | head -20"
    )[1])

    # Parse the probe's PID from /run/drm-master-probe.log.
    log_text = machine.succeed("cat /run/drm-master-probe.log")
    pid_match = re.search(r"phase=initramfs pid=(\d+)", log_text)
    if not pid_match:
        raise AssertionError(
            f"phase=initramfs line not found in /run/drm-master-probe.log\n{log_text}"
        )
    probe_pid = pid_match.group(1)
    print(f"probe PID = {probe_pid}")

    post_count = log_text.count("phase=post-switchroot setresuid")
    if post_count < 1:
        raise AssertionError(
            f"phase=post-switchroot line missing from log; probe died before reaching it\n{log_text}"
        )
    print(
        f"PASS: same PID {probe_pid} emitted phase=initramfs AND "
        f"phase=post-switchroot"
    )

    # Aliveness check + classify the death mode if dead.
    alive_check = machine.execute(f"kill -0 {probe_pid}")
    if alive_check[0] != 0:
        print(f"=== probe PID {probe_pid} is DEAD ===")
        events = machine.execute(
            "cat /run/drm-master-probe-events.log 2>/dev/null"
        )[1].strip()
        if events:
            print(f"--- signal that killed the probe: {events!r} ---")
        else:
            print(
                "--- no signal logged in /run/drm-master-probe-events.log: "
                "likely SIGKILL (cgroup.kill or similar) or Rust panic ---"
            )
        # Surface the last 30 lines of the probe log for forensic context.
        tail = machine.execute(
            "tail -30 /run/drm-master-probe.log"
        )[1]
        print(f"--- last 30 lines of /run/drm-master-probe.log ---\n{tail}")
        raise AssertionError(
            f"probe PID {probe_pid} died before assertions could run"
        )
    print(f"PASS: probe PID {probe_pid} still alive")

    # /proc/<pid>/status: all four Uid fields must be 1000 (the default
    # PROBE_DROP_UID). Format: "Uid:\t<real>\t<eff>\t<saved>\t<fs>"
    status_uid = machine.succeed(
        f"grep '^Uid:' /proc/{probe_pid}/status"
    ).strip()
    print(f"status Uid line: {status_uid}")
    uid_fields = status_uid.split()
    if len(uid_fields) < 5:
        raise AssertionError(
            f"unexpected /proc/{probe_pid}/status Uid format: {status_uid!r}"
        )
    if not all(f == "1000" for f in uid_fields[1:5]):
        raise AssertionError(
            f"expected all Uid fields = 1000; got {uid_fields[1:5]}"
        )
    print(f"PASS: probe PID {probe_pid} runs as UID 1000 (dropped from root)")

    # debugfs: probe PID must hold DRM master.
    drm_clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    print(f"DRM clients:\n{drm_clients}")
    found_master = False
    for line in drm_clients.splitlines():
        fields = line.split()
        # columns: command tgid dev master a uid magic name id
        if len(fields) >= 4 and fields[1] == probe_pid and fields[3] == "y":
            found_master = True
            break
    if not found_master:
        raise AssertionError(
            f"probe PID {probe_pid} is not DRM master after setresuid.\n"
            f"debugfs clients:\n{drm_clients}"
        )
    print(
        f"PASS: probe PID {probe_pid} still holds DRM master after setresuid"
    )

    # Continuous-tick assertion: consecutive t values in our log's tick
    # lines must never differ by more than 2 seconds — that's how we
    # detect "the process stalled or paused at switch_root."
    tick_output = machine.succeed(
        "grep -oE 'tick t=[0-9]+s' /run/drm-master-probe.log"
    ).strip()
    ticks = [
        int(m.group())
        for line in tick_output.splitlines()
        if (m := re.search(r"\d+", line))
    ]
    if len(ticks) < 5:
        raise AssertionError(f"expected at least 5 tick lines; got {len(ticks)}")
    max_gap = 0
    for i in range(1, len(ticks)):
        gap = ticks[i] - ticks[i - 1]
        if gap > max_gap:
            max_gap = gap
        if gap > 2:
            raise AssertionError(
                f"tick gap > 2s detected between t={ticks[i-1]}s and "
                f"t={ticks[i]}s — continuity broken across switch_root"
            )
    print(f"PASS: {len(ticks)} tick lines, max gap = {max_gap}s")

    # logind master conflicts (same as Phase 0).
    logind_errors = machine.succeed(
        "journalctl -u systemd-logind -p err..warning -b "
        "| grep -E 'master|TakeDevice|Failed to acquire' || true"
    ).strip()
    if logind_errors:
        raise AssertionError(f"systemd-logind reported errors:\n{logind_errors}")
    print("PASS: no systemd-logind master conflicts")

    print("drm-master-probe-phase1: ALL ASSERTIONS PASSED")
  '';
}
