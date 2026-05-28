# halmasuit-shutdown-probe Phase 0: validate `SurviveFinalKillSignal=yes`
# survives systemd-shutdown's kill spree on the ROOTFS side.
#
# Phase 2 of drm-master-probe answered the equivalent question for the
# BOOT-direction kill spree (initramfs→rootfs pivot): SurviveFinalKillSignal
# =yes works. The systemd documentation
# (https://www.freedesktop.org/software/systemd/man/systemd.unit.html) says
# the directive applies to "the final phase of the system shutdown
# process" — the SHUTDOWN-direction kill spree (rootfs→shutdownRamfs pivot).
# That is also where halmasuit needs to keep painting for Epic #47 R2.
#
# Mechanism:
#   * Probe writes timestamped heartbeats to /dev/kmsg every 100ms.
#   * Probe ignores SIGTERM/SIGINT/SIGHUP — only SIGKILL kills it.
#   * Probe runs under a rootfs systemd unit with SurviveFinalKillSignal=yes.
#   * testScript triggers `systemctl poweroff`, waits for VM halt, then
#     reads the qemu serial console capture (machine.get_console_log()).
#   * Assert at least one heartbeat line is captured AFTER systemd-shutdown's
#     "Sending SIGKILL to remaining processes" marker. That marker is the
#     last point at which a non-Surviving process gets killed; heartbeats
#     past it prove the directive worked.
#
# What this probe does NOT validate:
#   * Pivot survival to /run/initramfs (the actual shutdownRamfs). That's
#     Phase 1.
#   * DRM master persistence post-pivot. That's Phase 2.
#
# Phase 0 is the smallest possible step: unit-level survival of the rootfs
# kill spree. If this fails, none of Phase 1 or 2 is reachable; we'd fall
# back to the partial-scope alternative documented in Epic #47.

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
  name = "halmasuit-shutdown-probe-phase0";

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
      };

      # Rootfs systemd-managed probe unit. The hypothesis-under-test
      # directive is `SurviveFinalKillSignal=yes` in [Unit].
      systemd.services.halmasuit-shutdown-probe = {
        description = "Phase 0: SurviveFinalKillSignal=yes survives rootfs kill spree";
        wantedBy = [ "multi-user.target" ];
        # Run BEFORE shutdown.target so the unit is still active when
        # systemd-shutdown takes over. DefaultDependencies=no keeps
        # systemd from stopping us during the normal shutdown sequence
        # (we want to be killed by systemd-shutdown's final kill spree,
        # not by a clean unit stop).
        before = [ "shutdown.target" ];
        unitConfig = {
          DefaultDependencies = false;
          # Per systemd's load-fragment-gperf.gperf.in, this maps as
          # Unit.SurviveFinalKillSignal — NOT Service.* — and is silently
          # rejected with "Unknown key" if misplaced. Same gotcha that bit
          # drm-master-probe-phase2; same fix.
          SurviveFinalKillSignal = "yes";
        };
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${halmasuitShutdownProbe}/bin/halmasuit-shutdown-probe phase0";
          Restart        = "no";
          # KillMode=process means systemd only kills the main pid on
          # unit stop; SurviveFinalKillSignal protects against the
          # systemd-shutdown final kill, not against unit-level kill.
          KillMode       = "process";
          StandardOutput = "journal";
          StandardError  = "journal";
          # The probe needs to write to /dev/kmsg. CAP_SYSLOG is the
          # minimum capability for that; alternatively root (which
          # systemd default uses) suffices.
          AmbientCapabilities = [ "CAP_SYSLOG" ];
        };
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("halmasuit-shutdown-probe.service")

    # Confirm the probe is alive + emitting heartbeats BEFORE we trigger
    # shutdown. If this races, no shutdown test is meaningful.
    machine.wait_until_succeeds(
        "grep -q 'halmasuit-shutdown-probe\\[phase0\\]: heartbeat' /dev/kmsg",
        timeout=15
    )
    probe_pid = machine.succeed(
        "systemctl show -p MainPID halmasuit-shutdown-probe.service"
    ).strip().split("=")[1]
    print(f"PASS: probe PID = {probe_pid}, emitting heartbeats pre-shutdown")

    # Trigger shutdown. machine.shutdown() invokes `systemctl poweroff`
    # and waits for the qemu process to exit.
    machine.shutdown()

    # qemu is gone; the serial console buffer is finalized. Reads from
    # machine.get_console_log() now return everything qemu wrote to
    # stdio (the kernel console, including /dev/kmsg writes from the
    # probe) from boot to halt.
    console = machine.get_console_log()

    # Locate systemd-shutdown's "Sending SIGKILL to remaining processes"
    # marker. This is the last-line-of-defense kill before halt; a
    # process without SurviveFinalKillSignal=yes dies here. Heartbeats
    # AFTER this line prove the directive worked.
    #
    # systemd-shutdown's exact wording has varied across versions. Match
    # the SIGKILL line tolerantly.
    sigkill_re = re.compile(
        r"Sending SIGKILL to remaining processes",
        re.IGNORECASE
    )
    sigkill_pos = None
    for i, line in enumerate(console.splitlines()):
        if sigkill_re.search(line):
            sigkill_pos = i
            break
    if sigkill_pos is None:
        # systemd-shutdown's killing-of-processes log line is missing
        # entirely. This usually means the shutdown sequence terminated
        # before that point, OR systemd's log lines were filtered out of
        # the serial console capture. Dump the last 100 lines for
        # diagnosis.
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate 'Sending SIGKILL to remaining processes' "
            "marker in the serial console log. systemd-shutdown's "
            "kill-spree phase did not run, or its log line did not "
            "reach the console.\n\nLast 100 lines of console:\n" + tail
        )
    print(f"PASS: located SIGKILL kill-spree marker at console line {sigkill_pos}")

    # Count heartbeats AFTER the SIGKILL marker. >= 1 = survival
    # confirmed.
    hb_re = re.compile(r"halmasuit-shutdown-probe\[phase0\]: heartbeat")
    post_kill_hbs = 0
    last_post_kill_hb_seq = None
    seq_re = re.compile(r"heartbeat seq=(\d+)")
    for line in console.splitlines()[sigkill_pos + 1:]:
        if hb_re.search(line):
            post_kill_hbs += 1
            m = seq_re.search(line)
            if m:
                last_post_kill_hb_seq = m.group(1)

    if post_kill_hbs < 1:
        # FAIL: directive did not protect the process. The next 50 lines
        # after the SIGKILL marker are the diagnostic surface.
        post_kill_window = "\n".join(
            console.splitlines()[sigkill_pos:sigkill_pos + 50]
        )
        raise AssertionError(
            "Probe did NOT survive the kill spree: 0 heartbeats found "
            "after the SIGKILL marker. SurviveFinalKillSignal=yes is "
            "either misplaced or does not protect against systemd-"
            "shutdown's rootfs kill spree.\n\nWindow after SIGKILL "
            "marker:\n" + post_kill_window
        )
    print(
        f"PASS: {post_kill_hbs} heartbeat(s) emitted AFTER systemd-shutdown's "
        f"SIGKILL kill-spree marker (last seq={last_post_kill_hb_seq}); "
        "SurviveFinalKillSignal=yes WORKS on the rootfs side"
    )
  '';
}
