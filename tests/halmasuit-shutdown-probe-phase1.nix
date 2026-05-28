# halmasuit-shutdown-probe Phase 1: validate same-PID survival ACROSS
# systemd-shutdown's pivot to /run/initramfs.
#
# Phase 0 proved `SurviveFinalKillSignal=yes` keeps the process alive
# past the "Sending SIGKILL to remaining processes" kill spree. Phase 1
# proves the same PID survives the ACTUAL PIVOT into the shutdown
# initramfs (systemd-shutdown's exec into `/run/initramfs/shutdown` at
# the end of the kill spree).
#
# Mechanism:
#   * Probe runs the same heartbeat loop as Phase 0 (tagged "phase1").
#   * NixOS module also sets
#     `boot.initrd.systemd.shutdownRamfs.storePaths` to include the
#     probe binary + its closure, so the probe's mmap'd executable is
#     backed by the shutdown ramfs's tmpfs after rootfs unmount. (Phase
#     0 worked without this because all the relevant pages were
#     resident, but the architectural guarantee is shutdownRamfs.
#     Production halmasuit MUST be in shutdownRamfs.storePaths for the
#     same reason.)
#   * testScript triggers `systemctl poweroff`, waits for VM halt,
#     reads serial console.
#   * Assertion 1: at least one heartbeat appears AFTER the first
#     `shutdown[1]:` log line — that line is emitted by
#     systemd-shutdown AFTER it has exec'd into /run/initramfs/shutdown
#     (the post-pivot binary), so any heartbeat after it proves the
#     probe survived the pivot.
#   * Assertion 2: every heartbeat in the log carries the SAME `pid=N`
#     value. No PID change = same process throughout = no respawn.

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
  name = "halmasuit-shutdown-probe-phase1";

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

      # The architectural ask: the probe binary's executable + dynamic
      # linker dependencies must live in the shutdownRamfs tmpfs view
      # so the process can keep executing after rootfs unmounts. Phase
      # 0 worked without this (page cache held the relevant text
      # pages); Phase 1 makes the guarantee explicit. NixOS exposes
      # this as `systemd.shutdownRamfs.storePaths` (NOT
      # `boot.initrd.systemd.shutdownRamfs.*` — that's the BOOT
      # initramfs's systemd config; the shutdown initramfs is a
      # separate closure built by `generate-shutdown-ramfs.service`).
      systemd.shutdownRamfs.storePaths = [
        "${halmasuitShutdownProbe}/bin/halmasuit-shutdown-probe"
      ];

      systemd.services.halmasuit-shutdown-probe = {
        description = "Phase 1: same-PID survival across rootfs→shutdownRamfs pivot";
        wantedBy = [ "multi-user.target" ];
        before = [ "shutdown.target" ];
        unitConfig = {
          DefaultDependencies = false;
          # Same load-bearing directive as Phase 0; survival mechanic
          # is unchanged between sub-phases.
          SurviveFinalKillSignal = "yes";
        };
        serviceConfig = {
          Type           = "simple";
          ExecStart      = "${halmasuitShutdownProbe}/bin/halmasuit-shutdown-probe phase1";
          Restart        = "no";
          KillMode       = "process";
          StandardOutput = "journal";
          StandardError  = "journal";
          AmbientCapabilities = [ "CAP_SYSLOG" ];
        };
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("halmasuit-shutdown-probe.service")

    machine.wait_until_succeeds(
        "grep -q 'halmasuit-shutdown-probe\\[phase1\\]: heartbeat' /dev/kmsg",
        timeout=15
    )
    probe_pid = machine.succeed(
        "systemctl show -p MainPID halmasuit-shutdown-probe.service"
    ).strip().split("=")[1]
    print(f"PASS: probe PID = {probe_pid}, emitting heartbeats pre-shutdown")

    machine.shutdown()
    console = machine.get_console_log()

    # Locate the first `shutdown[1]:` line. That's emitted by
    # systemd-shutdown AFTER it has exec'd into /run/initramfs/shutdown
    # — i.e., post-pivot. systemd's own log lines pre-pivot look like
    # "systemd-shutdown[1]: ..." or "systemd[1]: ..." (with the
    # `systemd-` prefix); the post-pivot binary logs as plain
    # `shutdown[1]:`.
    pivot_re = re.compile(r"^[^a-zA-Z]*shutdown\[1\]:", re.MULTILINE)
    pivot_match = pivot_re.search(console)
    if pivot_match is None:
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate `shutdown[1]:` post-pivot marker in serial "
            "console. systemd-shutdown either did not pivot to "
            "/run/initramfs (shutdownRamfs.enable off?) or the log line "
            "did not reach the console.\n\nLast 100 console lines:\n" + tail
        )
    pivot_offset = pivot_match.start()
    pivot_line = console.count("\n", 0, pivot_offset)
    print(f"PASS: located post-pivot `shutdown[1]:` marker at console line {pivot_line}")

    # Count heartbeats AFTER the pivot marker. >= 1 means the probe
    # survived the pivot itself (not just the pre-pivot kill spree).
    post_pivot = console[pivot_offset:]
    hb_re = re.compile(
        r"halmasuit-shutdown-probe\[phase1\]: heartbeat seq=(\d+) pid=(\d+)"
    )
    post_pivot_hbs = list(hb_re.finditer(post_pivot))
    if not post_pivot_hbs:
        post_pivot_window = "\n".join(post_pivot.splitlines()[:50])
        raise AssertionError(
            "Probe did NOT survive the pivot: 0 heartbeats found "
            "after the first `shutdown[1]:` marker. The probe's "
            "mmap'd executable / libraries may have been unmapped "
            "when rootfs unmounted — check that the binary + its "
            "closure are in boot.initrd.systemd.shutdownRamfs.storePaths."
            "\n\nWindow after pivot marker:\n" + post_pivot_window
        )
    last_post = post_pivot_hbs[-1]
    print(
        f"PASS: {len(post_pivot_hbs)} heartbeat(s) emitted AFTER the "
        f"post-pivot `shutdown[1]:` marker; last seq={last_post.group(1)}"
    )

    # All heartbeats (both pre- and post-pivot) must carry the SAME
    # pid. Different pid = process respawned, not survived. Crawl
    # every heartbeat in the WHOLE console (not just post-pivot) and
    # confirm one unique pid.
    all_hbs = list(hb_re.finditer(console))
    pids = {m.group(2) for m in all_hbs}
    if len(pids) != 1:
        raise AssertionError(
            f"Heartbeats carry MULTIPLE pids: {sorted(pids)}. The probe "
            "respawned mid-shutdown (lost SurviveFinalKillSignal contract) "
            "or another process is impersonating heartbeats."
        )
    surviving_pid = next(iter(pids))
    if surviving_pid != probe_pid:
        raise AssertionError(
            f"Heartbeats carry pid={surviving_pid} but systemd's MainPID "
            f"reported probe_pid={probe_pid}. PID continuity invariant violated."
        )
    print(
        f"PASS: every heartbeat (pre- and post-pivot, {len(all_hbs)} total) "
        f"carries pid={surviving_pid} — same PID throughout"
    )
  '';
}
