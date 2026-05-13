# Phase 3 research probe: validate exec across switch_root + DRM fd
# preservation.
#
# Hypothesis: halmasuit-in-initramfs can `execve` into the rootfs-resident
# binary path BEFORE switch_root, preserve its DRM master fd across the
# exec (by clearing FD_CLOEXEC), and the post-exec process can continue
# Phase 1/2 flow — surviving the killall via SurviveFinalKillSignal=yes,
# dropping privileges post-pivot, holding DRM master throughout.
#
# Phase 3 composes WITH Phase 2's survival mechanism — exec doesn't
# replace SurviveFinalKillSignal=yes, it adds canonical-rootfs-path
# semantics on top. Reason: the killall iterates by PID; exec preserves
# PID; we still need to be on the killall skip list.
#
# Probe-side mechanism (env-var-driven):
#   PROBE_EXEC_AT_INIT=1        → pre-exec branch (immediately re-exec to /sysroot/<own path>)
#   PROBE_EXEC_PASS=post        → post-exec branch (inherit DRM fd from PROBE_DRM_FD env)
#   PROBE_DRM_FD=<n>            → the inherited fd number
#   PROBE_SKIP_ARGV0_MARK=1     → use SurviveFinalKillSignal=yes for survival
#
# The unit starts with PROBE_EXEC_AT_INIT=1 + PROBE_SKIP_ARGV0_MARK=1.
# The pre-exec branch re-execs itself with PROBE_EXEC_PASS=post and
# PROBE_DRM_FD=<fd>; the post-exec process is what survives switch_root.

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
  name = "drm-master-probe-phase3";

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

      # Phase 3 specifically needs the probe binary in the ROOTFS too,
      # because the pre-exec branch execs to /sysroot/<own path>. NixOS's
      # boot.initrd.systemd.storePaths only ships the binary in the
      # initramfs; without environment.systemPackages the same store
      # path doesn't exist when the rootfs is mounted at /sysroot.
      # Production halmasuit will be in both via the production NixOS
      # module; the research probe needs it explicit.
      environment.systemPackages = [ drmMasterProbe ];

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
        description = "Phase 3 DRM master persistence probe (exec across switch_root)";
        wantedBy = [ "initrd.target" ];
        # initrd-fs.target is the systemd-documented "all rootfs mounts
        # done" target — fires after sysroot.mount AND all the
        # /sysroot/nix/.{ro,rw}-store overlay mounts that NixOS sets up.
        # `after = sysroot.mount` alone is not enough: the probe would
        # run between /sysroot mount and the nix-store overlay mounts,
        # so /sysroot/nix/.ro-store/<hash>/... wouldn't exist yet.
        after    = [
          "systemd-modules-load.service"
          "systemd-udev-settle.service"
          "drm-diag.service"
          "initrd-fs.target"
        ];
        requires = [ "initrd-fs.target" ];
        before   = [ "initrd-switch-root.service" ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate     = true;
          SurviveFinalKillSignal = "yes";
        };
        environment = {
          PROBE_SKIP_ARGV0_MARK = "1";
          PROBE_EXEC_AT_INIT    = "1";
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

    print("=== /run/drm-master-probe.log ===")
    print(machine.succeed("cat /run/drm-master-probe.log"))
    print("=== /run/drm-master-probe-events.log ===")
    print(machine.execute("cat /run/drm-master-probe-events.log 2>&1")[1])
    print("=== systemctl status drm-master-probe.service ===")
    print(machine.execute(
        "systemctl status drm-master-probe.service 2>&1 || true"
    )[1])

    # Sanity: directive parse check (same defensive assertion as Phase 2).
    parse_warn = machine.execute(
        "journalctl -b | grep -i 'Unknown key.*SurviveFinalKillSignal' || true"
    )[1].strip()
    if parse_warn:
        raise AssertionError(
            "systemd rejected SurviveFinalKillSignal as 'Unknown key' — directive "
            "was silently ignored. The probe's survival is NOT evidence for the "
            "Phase 3 hypothesis under those conditions.\n"
            f"journal:\n{parse_warn}"
        )
    print("PASS: systemd parsed SurviveFinalKillSignal correctly")

    log_text = machine.succeed("cat /run/drm-master-probe.log")

    # Phase 3 must have logged TWO `phase=initramfs` lines — one from the
    # pre-exec process (mechanism=exec-pre), one from the post-exec
    # process (mechanism=exec-post). Both should carry the SAME PID
    # because execve preserves PID.
    initramfs_lines = re.findall(
        r"phase=initramfs pid=(\d+) mechanism=(\S+)", log_text
    )
    if len(initramfs_lines) < 2:
        raise AssertionError(
            "expected at least 2 'phase=initramfs' lines (pre-exec + post-exec); "
            f"got {len(initramfs_lines)}:\n{initramfs_lines}\nFull log:\n{log_text}"
        )

    pre_exec = [(p, m) for (p, m) in initramfs_lines if m == "exec-pre"]
    post_exec = [(p, m) for (p, m) in initramfs_lines if m == "exec-post"]
    if not pre_exec:
        raise AssertionError(
            f"no 'mechanism=exec-pre' line; pre-exec branch did not run.\n{log_text}"
        )
    if not post_exec:
        raise AssertionError(
            "no 'mechanism=exec-post' line; the exec did not produce a post-exec "
            f"process.\n{log_text}"
        )
    pre_pid = pre_exec[0][0]
    post_pid = post_exec[0][0]
    if pre_pid != post_pid:
        raise AssertionError(
            f"execve must preserve PID; pre-exec PID={pre_pid} post-exec PID={post_pid}"
        )
    probe_pid = pre_pid
    print(
        f"PASS: pre-exec PID {pre_pid} == post-exec PID {post_pid} "
        "— execve preserved PID across the process-image swap"
    )

    # Phase 3-specific: post-exec must have verified mastery via SET_MASTER.
    if "post-exec SET_MASTER ok — mastery survived exec" not in log_text:
        raise AssertionError(
            "post-exec branch did not log SET_MASTER success — DRM mastery may not "
            f"have survived the exec.\nFull log:\n{log_text}"
        )
    print("PASS: post-exec SET_MASTER succeeded — DRM mastery survived execve")

    # Phase 3-specific: post-exec must have rediscovered the FB via GETCRTC.
    if "post-exec rediscovered" not in log_text:
        raise AssertionError(
            "post-exec branch did not log rediscovered handles — the kernel may "
            f"not have retained the CRTC↔FB binding across exec.\nFull log:\n{log_text}"
        )
    print("PASS: post-exec re-derived CRTC/FB/connector handles from kernel state")

    # Inheriting Phase 1/2 downstream assertions — the post-exec process
    # must reach the post-switchroot phase and pass setresuid + master-
    # held + tick-continuity checks.
    if "phase=post-switchroot setresuid" not in log_text:
        events = machine.execute(
            "cat /run/drm-master-probe-events.log 2>/dev/null"
        )[1].strip()
        tail = machine.execute("tail -30 /run/drm-master-probe.log")[1]
        raise AssertionError(
            "post-exec process did not reach phase=post-switchroot.\n"
            f"events: {events!r}\nlog tail:\n{tail}"
        )
    print(f"PASS: PID {probe_pid} reached phase=post-switchroot after exec + survival")

    # PID still alive.
    alive_check = machine.execute(f"kill -0 {probe_pid}")
    if alive_check[0] != 0:
        events = machine.execute(
            "cat /run/drm-master-probe-events.log 2>/dev/null"
        )[1].strip()
        raise AssertionError(
            f"probe PID {probe_pid} died after assertions.\nevents: {events!r}"
        )
    print(f"PASS: probe PID {probe_pid} still alive")

    # /proc/<pid>/exe should NOT start with /sysroot — pivot_root must
    # have relocated the exe path. NixOS's overlay nix store means the
    # exact path can be either /nix/store/<hash>/... (post-pivot overlay)
    # OR /nix/.ro-store/<hash>/... (the underlying read-only mount the
    # exec resolved through). Both are valid post-pivot canonical paths;
    # the kernel records whichever was used at exec time.
    exe_path = machine.succeed(
        f"readlink /proc/{probe_pid}/exe"
    ).strip()
    print(f"/proc/{probe_pid}/exe = {exe_path}")
    if exe_path.startswith("/sysroot"):
        raise AssertionError(
            f"/proc/{probe_pid}/exe still starts with /sysroot — pivot_root "
            f"didn't relocate the exe path. Got: {exe_path}"
        )
    if not (
        exe_path.startswith("/nix/store/") or exe_path.startswith("/nix/.ro-store/")
    ):
        raise AssertionError(
            f"/proc/{probe_pid}/exe is not in /nix/store or /nix/.ro-store — "
            f"unexpected path. Got: {exe_path}"
        )
    print(f"PASS: /proc/{probe_pid}/exe = {exe_path} (post-pivot, no /sysroot prefix)")

    # Privilege drop verified by checking /proc/<pid>/status.
    status_uid = machine.succeed(
        f"grep '^Uid:' /proc/{probe_pid}/status"
    ).strip()
    uid_fields = status_uid.split()
    if not all(f == "1000" for f in uid_fields[1:5]):
        raise AssertionError(
            f"expected all Uid fields = 1000; got {uid_fields[1:5]}"
        )
    print(f"PASS: probe PID {probe_pid} runs as UID 1000 (dropped from root)")

    # Master still held.
    drm_clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    found_master = False
    for line in drm_clients.splitlines():
        fields = line.split()
        if len(fields) >= 4 and fields[1] == probe_pid and fields[3] == "y":
            found_master = True
            break
    if not found_master:
        raise AssertionError(
            f"probe PID {probe_pid} is not DRM master after exec + setresuid.\n"
            f"debugfs clients:\n{drm_clients}"
        )
    print(f"PASS: probe PID {probe_pid} still holds DRM master")

    # Tick continuity ≤2s gap across the whole timeline.
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
                f"t={ticks[i]}s — continuity broken across exec or switch_root"
            )
    print(f"PASS: {len(ticks)} tick lines, max gap = {max_gap}s")

    # logind clean.
    logind_errors = machine.succeed(
        "journalctl -u systemd-logind -p err..warning -b "
        "| grep -E 'master|TakeDevice|Failed to acquire' || true"
    ).strip()
    if logind_errors:
        raise AssertionError(f"systemd-logind reported errors:\n{logind_errors}")
    print("PASS: no systemd-logind master conflicts")

    print(
        "drm-master-probe-phase3: exec across switch_root + DRM fd "
        "preservation EMPIRICALLY VERIFIED"
    )
  '';
}
