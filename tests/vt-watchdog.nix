# Epic #71 R-honest.8 — systemd watchdog recovery.
#
# Proves the recovery complement that makes compositor-owned VT_PROCESS
# safe: a HUNG-but-alive halmasuit is detected and killed by systemd's
# watchdog, then restarted. (A DEAD VT_PROCESS controller is already
# safe — the kernel's reset_vc reverts its VT to VT_AUTO; only the
# hung-but-alive case needs the watchdog to convert hung→dead.)
#
# halmasuit pings WATCHDOG=1 to $NOTIFY_SOCKET from its calloop loop at
# WatchdogSec/2. The test:
#   1. Confirms a HEALTHY halmasuit survives well past WatchdogSec — it
#      reaches scanout_active and keeps its MainPID, which only happens
#      if its pings actually reach systemd ($NOTIFY_SOCKET wired right).
#   2. SIGSTOPs the main process — a genuine event-loop freeze, no test
#      hook in production code — so the pings stop.
#   3. Asserts systemd logs a watchdog timeout, SIGKILLs the frozen
#      instance, and Restart=on-failure brings up a NEW MainPID.
#
# watchdogSec is set to 25s here (vs the 30s production default): long
# enough that halmasuit's cold DRM/EGL bring-up (~13s) never trips it,
# short enough to keep the test bounded. TimeoutStopSec is forced to 5s
# so the SIGKILL escalation after the (pending, because stopped) SIGTERM
# doesn't wait the 90s default.
#
# State-based polling throughout (wait_until_succeeds).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  testGreeter = pkgs.writeShellScript "halmasuit-watchdog-greeter" ''
    exec ${pkgs.coreutils}/bin/sleep infinity
  '';
in
pkgs.testers.runNixOSTest {
  name = "vt-watchdog";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        greeterCommand  = "${testGreeter}";
        # Shorter than the 30s default but still well above cold-start.
        watchdogSec     = "25s";
        wallpaper = {
          type   = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
      };

      # Force a short stop timeout so the SIGKILL escalation after the
      # watchdog fires (the stopped process can't act on SIGTERM) lands
      # in seconds, not the 90s default — keeps the test bounded.
      systemd.services.halmasuit.serviceConfig.TimeoutStopSec = lib.mkForce "5s";

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
      users.users.test.group = "test";
      users.groups.test.gid  = 1000;

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=60
    )

    # ── 1. Healthy halmasuit survives past the watchdog interval ──
    # Reaching scanout_active (~13s) without a restart, plus a stable
    # MainPID, proves its WATCHDOG=1 pings reach systemd — if
    # $NOTIFY_SOCKET were misconfigured, systemd would have killed it at
    # WatchdogSec and we'd see a different (restarted) PID.
    old_pid = machine.succeed(
        "systemctl show --value -p MainPID halmasuit.service"
    ).strip()
    assert old_pid not in ("", "0"), f"no halmasuit MainPID: {old_pid!r}"
    print(f"halmasuit healthy past watchdog interval, MainPID={old_pid}")

    # ── 2. Freeze the event loop → pings stop ──
    # SIGSTOP halts every thread (including the calloop thread), so the
    # WATCHDOG=1 pings stop — a genuine hang, no production test hook.
    machine.succeed(f"kill -STOP {old_pid}")
    print(f"SIGSTOP sent to {old_pid}; awaiting systemd watchdog")

    # ── 3. systemd watchdog kills + Restart=on-failure recovers ──
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qiF 'watchdog timeout'", timeout=60
    )
    machine.wait_until_succeeds(
        'p="$(systemctl show --value -p MainPID halmasuit.service)"; '
        f'[ "$p" != "{old_pid}" ] && [ "$p" != "0" ] && [ -n "$p" ]',
        timeout=60,
    )
    machine.wait_for_unit("halmasuit.service")
    new_pid = machine.succeed(
        "systemctl show --value -p MainPID halmasuit.service"
    ).strip()
    assert new_pid not in ("", "0") and new_pid != old_pid, (
        f"expected a restarted MainPID distinct from {old_pid}, got {new_pid!r}"
    )

    print("=" * 70)
    print("PASS: systemd watchdog detected the frozen (SIGSTOP) compositor,")
    print(f"      SIGKILLed it, and Restart=on-failure recovered it: "
          f"{old_pid} → {new_pid}.")
    print("=" * 70)
  '';
}
