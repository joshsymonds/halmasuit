# Epic #71 R-honest.7 — cooperative VT-switch round-trip (home-VT model).
#
# Proves the REAL halmasuit compositor's cooperative VT loop end-to-end.
# halmasuit owns its HOME vt (tty8) as the kernel's VT_PROCESS
# controller — opened in its root startup window, brought to the
# foreground at startup. A switch is a plain VT_ACTIVATE(target) on that
# controlling-tty fd; the kernel sends the cooperative relsig (a
# REALTIME signal — SIGUSR1/2 get stolen by Mesa/EGL threads,
# freedesktop #87322) to halmasuit's dedicated signalfd, which drops DRM
# master + VT_RELDISP(release); switching back fires acqsig → reacquire
# + VT_RELDISP(ackacq).
#
#   1. startup → halmasuit becomes tty8's VT_PROCESS controller and
#      VT_ACTIVATEs it → active=tty8.
#   2. send_key Ctrl+Alt+F2 → halmasuit (foreground) VT_ACTIVATE(2) →
#      kernel relsig(tty8) → "VT_RELSIG_HANDLED" (DRM paused +
#      VT_RELDISP release) → switch completes → active=tty2 (a getty,
#      the recovery console — untouched by halmasuit).
#   3. chvt 8 → kernel acqsig(tty8) → "VT_ACQSIG_HANDLED" (DRM resumed +
#      VT_RELDISP ackacq) → active=tty8.
#
# home VT = tty8 is outside logind's autovt range (NAutoVTs=6), so no
# getty ever competes for it — the deployment invariant the home-VT
# model requires. Switching TO tty2 lets tty2's getty come up normally;
# that getty's vhangup() no longer revokes halmasuit's fd because
# halmasuit never grabbed tty2 (the bug the home-VT model fixes).
#
# Asserts journal markers AND kernel VT state (/sys/class/tty/tty0/
# active) — both observable in headless (no pixels needed). State-based
# polling throughout (wait_until_succeeds).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  testGreeter = pkgs.writeShellScript "halmasuit-vtcoop-greeter" ''
    exec ${pkgs.coreutils}/bin/sleep infinity
  '';
in
pkgs.testers.runNixOSTest {
  name = "vt-cooperative-switch";

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
        # The home VT halmasuit owns as VT_PROCESS controller. tty8 is
        # outside the autovt range so no getty competes for it.
        homeVt          = 8;
        wallpaper = {
          type   = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
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
      users.users.test.group = "test";
      users.groups.test.gid  = 1000;

      environment.systemPackages = [ pkgs.kbd ]; # chvt

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          "-device virtio-keyboard-pci"
        ];
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'", timeout=30
    )

    # ── 0. halmasuit owns tty8 and brought it to the foreground ──
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'home VT opened; halmasuit is its VT_PROCESS controller'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        '[ "$(cat /sys/class/tty/tty0/active)" = tty8 ]', timeout=30
    )
    print("halmasuit owns tty8 (VT_PROCESS) and it is the foreground VT")

    # ── 1. Chord-driven switch AWAY to tty2 → relsig → RELEASE ──
    # halmasuit is foreground (tty8) so the chord reaches its libinput.
    # VT_ACTIVATE(2) → kernel relsig(tty8) → halmasuit drops DRM master
    # + VT_RELDISP(release) → the switch to tty2 completes (tty2's getty
    # comes up, untouched). THE load-bearing assertion: the cooperative
    # release handshake ran (it does NOT grab tty2, so no vhangup/EIO).
    machine.send_key("ctrl-alt-f2")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'VT_RELSIG_HANDLED'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        '[ "$(cat /sys/class/tty/tty0/active)" = tty2 ]', timeout=30
    )
    print("relsig handled: DRM paused + VT_RELDISP(release) → switched to tty2 (getty)")

    # ── 2. Switch BACK (chvt 8) → acqsig → reacquire ──
    # The kernel switches tty2→tty8; tty8 is VT_PROCESS so it sends
    # acqsig to halmasuit → reacquire DRM master + VT_RELDISP(ackacq).
    machine.succeed("timeout 15 chvt 8 || true")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'VT_ACQSIG_HANDLED'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        '[ "$(cat /sys/class/tty/tty0/active)" = tty8 ]', timeout=30
    )
    print("acqsig handled: DRM resumed + VT_RELDISP(ackacq) → back on tty8")

    print("=" * 70)
    print("PASS: home-VT cooperative loop — halmasuit owns tty8, a chord")
    print("      VT_ACTIVATE(2) drove relsig→release→tty2, and chvt 8 drove")
    print("      acqsig→reacquire→tty8 (markers + kernel VT state asserted).")
    print("=" * 70)
  '';
}
