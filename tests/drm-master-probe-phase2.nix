# Phase 2 research probe: test whether systemd's SurviveFinalKillSignal=yes
# unit directive (v255+) is a viable replacement for the @argv[0] storage-
# daemon convention used by Phase 1.
#
# Motivation: the @argv[0] convention is documented by systemd upstream as
# "storage technology only" (see systemd.io/ROOT_STORAGE_DAEMONS) and has
# seen recent regressions (systemd #37700, #40933). The team uses it for a
# non-storage daemon — halmasuit, a display server. Validating an
# alternative supported by upstream gives us either an upgrade path
# (Phase 2 green) or a documented justification for keeping @argv[0]
# (Phase 2 red).
#
# Open empirical question: SurviveFinalKillSignal= is documented for
# systemd-shutdown's killall at end-of-session shutdown. Does it also
# apply to systemd-shutdown's killall during initramfs→rootfs pivot?
# This probe answers that.
#
# Difference from Phase 1:
#   - DROP the @argv[0] write inside the probe (PROBE_SKIP_ARGV0_MARK=1)
#   - ADD SurviveFinalKillSignal=yes to the unit's serviceConfig
# Everything else is identical so the comparison is clean.

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
  name = "drm-master-probe-phase2";

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

      boot.initrd.systemd.enable = true;
      boot.initrd.availableKernelModules = [ "virtio_gpu" ];
      boot.initrd.kernelModules = [ "virtio_gpu" ];

      boot.initrd.systemd.storePaths = [
        "${drmMasterProbe}/bin/drm-master-probe"
      ];

      # Phase 2 survival mechanism: SurviveFinalKillSignal=yes on the
      # unit, NO argv[0] mark inside the probe. Keep DefaultDependencies
      # and IgnoreOnIsolate as Phase 1 sets them — those control whether
      # systemd STOPS the unit during isolation, which is orthogonal to
      # whether systemd-shutdown's killall kills our PID during pivot.
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
        description = "Phase 2 DRM master persistence probe (SurviveFinalKillSignal=yes)";
        wantedBy = [ "initrd.target" ];
        after    = [ "systemd-modules-load.service" "systemd-udev-settle.service" "drm-diag.service" ];
        before   = [ "initrd-switch-root.service" ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate     = true;
          # The hypothesis under test. Belongs in [Unit] per systemd's
          # load-fragment-gperf.gperf.in (Unit.SurviveFinalKillSignal,
          # NOT Service.SurviveFinalKillSignal — got this wrong first try
          # and systemd silently rejected it with "Unknown key").
          SurviveFinalKillSignal = "yes";
        };
        environment = {
          PROBE_SKIP_ARGV0_MARK = "1";
        };
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${drmMasterProbe}/bin/drm-master-probe";
          Restart        = "no";
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

    time.sleep(10)

    # CRITICAL: the directive must actually have been parsed. If systemd
    # silently rejects "Unknown key 'SurviveFinalKillSignal'" then any
    # apparent success is via OTHER mechanisms (the SIGTERM-ignore
    # handler, DefaultDependencies=false, etc.) and we're NOT testing
    # the hypothesis we think we're testing.
    parse_warn = machine.execute(
        "journalctl -b | grep -i 'Unknown key.*SurviveFinalKillSignal' || true"
    )[1].strip()
    if parse_warn:
        raise AssertionError(
            "systemd rejected SurviveFinalKillSignal as 'Unknown key' — directive "
            "was silently ignored. The probe's survival (if any) is NOT evidence "
            "for the hypothesis. Check the unit's section placement: the directive "
            "lives in [Unit], not [Service] (Nix: unitConfig, not serviceConfig).\n"
            f"journal:\n{parse_warn}"
        )
    print("PASS: systemd did NOT reject SurviveFinalKillSignal — directive parsed")

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

    # Pull the probe's PID + mechanism tag from the log.
    log_text = machine.succeed("cat /run/drm-master-probe.log")
    pid_match = re.search(r"phase=initramfs pid=(\d+) mechanism=(\S+)", log_text)
    if not pid_match:
        raise AssertionError(
            f"phase=initramfs line not found in /run/drm-master-probe.log\n{log_text}"
        )
    probe_pid = pid_match.group(1)
    mechanism = pid_match.group(2)
    print(f"probe PID = {probe_pid}, mechanism = {mechanism}")
    if mechanism != "survivefinalkillsignal":
        raise AssertionError(
            f"expected mechanism=survivefinalkillsignal, got mechanism={mechanism!r}; "
            "probe didn't honor PROBE_SKIP_ARGV0_MARK"
        )

    # Verify the argv[0] mark is NOT present — proves the alternative
    # mechanism (not @argv[0]) is what would have kept us alive.
    # /proc/<pid>/cmdline is NUL-separated; the first byte must NOT be '@'.
    cmdline_first = machine.execute(
        f"head -c 1 /proc/{probe_pid}/cmdline 2>/dev/null || echo NOACCESS"
    )[1].strip()
    if cmdline_first == "@":
        raise AssertionError(
            "argv[0] starts with '@' — the probe is using the legacy mechanism, "
            "not testing SurviveFinalKillSignal=yes in isolation"
        )
    print(f"PASS: /proc/{probe_pid}/cmdline first byte is not '@' (got {cmdline_first!r})")

    # Did the probe reach the post-switchroot setresuid step?
    post_count = log_text.count("phase=post-switchroot setresuid")
    if post_count < 1:
        # Phase 2 FAILED. Collect forensics before raising.
        print("=== PHASE 2 RESULT: SurviveFinalKillSignal=yes did NOT survive switch_root ===")
        events = machine.execute(
            "cat /run/drm-master-probe-events.log 2>/dev/null"
        )[1].strip()
        if events:
            print(f"--- signal that killed the probe: {events!r} ---")
        else:
            print(
                "--- no signal logged: likely SIGKILL (systemd-shutdown's "
                "final SIGKILL is uncatchable) or process exited some other way ---"
            )
        alive = machine.execute(f"kill -0 {probe_pid} 2>&1")[0]
        print(f"--- kill -0 {probe_pid} → {'alive' if alive == 0 else 'dead'} ---")
        tail = machine.execute("tail -30 /run/drm-master-probe.log")[1]
        print(f"--- last 30 lines of probe log ---\n{tail}")
        raise AssertionError(
            "probe did not reach phase=post-switchroot — "
            "SurviveFinalKillSignal=yes is NOT a drop-in replacement for @argv[0] "
            "in the switch_root killall path. Keep using @argv[0] for now."
        )
    print(
        f"PASS: same PID {probe_pid} emitted phase=initramfs AND "
        "phase=post-switchroot using SurviveFinalKillSignal=yes"
    )

    # From here, all the same assertions as Phase 1 — we're checking
    # that the SurviveFinalKillSignal-protected probe drops privileges
    # and retains master correctly post-pivot.
    alive_check = machine.execute(f"kill -0 {probe_pid}")
    if alive_check[0] != 0:
        events = machine.execute(
            "cat /run/drm-master-probe-events.log 2>/dev/null"
        )[1].strip()
        tail = machine.execute("tail -30 /run/drm-master-probe.log")[1]
        raise AssertionError(
            f"probe PID {probe_pid} died after reaching post-switchroot.\n"
            f"events: {events!r}\nlog tail:\n{tail}"
        )
    print(f"PASS: probe PID {probe_pid} still alive")

    status_uid = machine.succeed(
        f"grep '^Uid:' /proc/{probe_pid}/status"
    ).strip()
    uid_fields = status_uid.split()
    if not all(f == "1000" for f in uid_fields[1:5]):
        raise AssertionError(
            f"expected all Uid fields = 1000; got {uid_fields[1:5]}"
        )
    print(f"PASS: probe PID {probe_pid} runs as UID 1000 (dropped from root)")

    drm_clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    found_master = False
    for line in drm_clients.splitlines():
        fields = line.split()
        if len(fields) >= 4 and fields[1] == probe_pid and fields[3] == "y":
            found_master = True
            break
    if not found_master:
        raise AssertionError(
            f"probe PID {probe_pid} is not DRM master after setresuid.\n"
            f"debugfs clients:\n{drm_clients}"
        )
    print(f"PASS: probe PID {probe_pid} still holds DRM master after setresuid")

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

    logind_errors = machine.succeed(
        "journalctl -u systemd-logind -p err..warning -b "
        "| grep -E 'master|TakeDevice|Failed to acquire' || true"
    ).strip()
    if logind_errors:
        raise AssertionError(f"systemd-logind reported errors:\n{logind_errors}")
    print("PASS: no systemd-logind master conflicts")

    print("drm-master-probe-phase2: SurviveFinalKillSignal=yes WORKS as @argv[0] replacement")
  '';
}
