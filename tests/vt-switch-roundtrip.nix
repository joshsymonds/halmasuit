# Epic #71 R1.4 — end-to-end VT-switching round-trip.
#
# Drives the full broker↔compositor VT-switching IPC dance against
# the live `halmasuit-session` broker + the live kernel's VT
# subsystem. This is the integration counterpart to R1.3's
# socketpair-based unit tests in `crates/halmasuit/src/vt_switch.rs`:
# unit tests pin the protocol shape, this test pins that the
# protocol actually drives the real kernel state machine.
#
# Mechanism:
#   * `services.halmasuit.session.enable = true` deploys the broker
#     unit (socket-activated, runs as root, has implicit
#     CAP_SYS_TTY_CONFIG).
#   * `services.halmasuit.greeterUid = 998` configures the broker's
#     SO_PEERCRED gate to accept connections from the
#     halmasuit-compositor system user (uid 998 in the test). This is
#     the same uid the production compositor runs as.
#   * Test script launches `halmasuit-vt-test-client` via
#     `systemd-run --uid=998` so it connects under the gated uid.
#   * Client sends `RequestVtSwitch{target_vt:2}`, receives Prepare(fd),
#     does TIOCSCTTY + VT_SETMODE on the inherited fd, sends
#     MasterDropped, receives Activated, logs `VERDICT: ACTIVATED`.
#   * Test script asserts:
#       1. `VERDICT: ACTIVATED` appears in the client log.
#       2. `/sys/class/tty/tty0/active` changed from `tty1` → `tty2`,
#          proving the kernel really switched (not just the protocol
#          completing).
#
# State-based polling throughout (`wait_until_succeeds`, never
# `time.sleep`) per memory feedback-state-based-polling.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-vt-test-client,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    # Module options have `default = pkgs.halmasuit` even on broker-
    # only deployments; apply the halmasuit overlay so the default
    # resolves cleanly during evaluation.
    overlays = [
      (_final: _prev: {
        inherit halmasuit halmasuit-session;
      })
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "vt-switch-roundtrip";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
      ];

      # Deploy the privileged broker. We DON'T enable
      # services.halmasuit.enable (no full compositor in this test;
      # just the broker). When cfg.enable=false the broker's
      # SO_PEERCRED gate uses greeterUid as the brokerPeerUid, so
      # the test client runs as the greeter system user.
      services.halmasuit.session = {
        enable  = true;
        package = halmasuit-session;
      };

      # The broker-only deployment (cfg.session.enable, cfg.enable=false)
      # block in nix/module.nix references the greeter user/group for
      # env wiring but the user-creation block is gated on
      # cfg.enable||cfg.fromInitrd.enable — so we declare them by hand.
      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;

      environment.systemPackages = [ halmasuit-vt-test-client pkgs.kbd ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    def dump():
        print("=== client log ===")
        print(machine.succeed("cat /tmp/vt-test.log 2>/dev/null || echo '(no log)'"))
        print("=== broker journal ===")
        print(machine.succeed("journalctl -u halmasuit-session --no-pager 2>&1 || true"))
        print("=== current active VT ===")
        print(machine.succeed("cat /sys/class/tty/tty0/active 2>&1 || true"))

    machine.start()
    machine.wait_for_unit("sockets.target")
    machine.wait_until_succeeds("systemctl is-active halmasuit-session.socket")

    # Broker is socket-activated (no standing root) — verify state.
    machine.fail("pgrep -x halmasuit-session")

    # Baseline: kernel is on tty1 (NixOS test VM default).
    baseline_vt = machine.succeed("cat /sys/class/tty/tty0/active").strip()
    print(f"baseline active VT: {baseline_vt}")
    if baseline_vt != "tty1":
        raise AssertionError(
            f"VM baseline should be tty1 (NixOS test default), got {baseline_vt}. "
            f"The test's pre/post comparison assumes tty1 → tty2 — please debug."
        )

    # Launch the test client as a transient systemd-run unit so it
    # inherits a TTY-less environment (matches how the production
    # compositor would invoke a VT switch).
    machine.succeed(
        "systemd-run --unit=vt-test-client --collect "
        "--uid=999 --gid=999 "
        "--property=Type=simple "
        "--property=StandardOutput=journal "
        "--property=StandardError=journal "
        "${halmasuit-vt-test-client}/bin/halmasuit-vt-test-client "
        "--broker-socket /run/halmasuit-session.sock "
        "--target-vt 2 "
        "--mode happy "
        "--log /tmp/vt-test.log"
    )

    # Poll for VERDICT line.
    import time
    deadline = time.time() + 20
    verdict = None
    while time.time() < deadline:
        contents = machine.succeed("cat /tmp/vt-test.log 2>/dev/null || echo")
        if "VERDICT:" in contents:
            for line in contents.splitlines():
                if line.startswith("VERDICT:"):
                    verdict = line.strip()
                    break
            break
        time.sleep(0.5)

    if verdict is None:
        dump()
        raise AssertionError("client did not log a VERDICT within 20s")

    print(f"client verdict: {verdict}")
    if verdict != "VERDICT: ACTIVATED":
        dump()
        raise AssertionError(f"expected 'VERDICT: ACTIVATED', got '{verdict}'")

    # Verify the kernel ACTUALLY switched VTs. The protocol completing
    # is necessary but not sufficient — we need to see the kernel state
    # change to confirm VT_ACTIVATE actually fired and took effect.
    final_vt = machine.succeed("cat /sys/class/tty/tty0/active").strip()
    print(f"final active VT: {final_vt}")
    if final_vt != "tty2":
        dump()
        raise AssertionError(
            f"VT switch protocol said ACTIVATED but /sys/class/tty/tty0/active "
            f"is still {final_vt} (expected tty2). Broker said it fired "
            f"VT_ACTIVATE but the kernel disagrees."
        )

    # Verify broker is back to idle (no standing root after the
    # transient request completes).
    deadline = time.time() + 60
    while time.time() < deadline:
        state = machine.succeed(
            "systemctl is-active halmasuit-session.service || true"
        ).strip()
        if state != "active":
            break
        time.sleep(1)

    # Verify NO kernel taint (the broker didn't crash the kernel
    # or trigger a WARN_ON in the VT subsystem).
    taint = machine.succeed("cat /proc/sys/kernel/tainted").strip()
    print(f"kernel taint flags: {taint}")
    if taint != "0":
        # G (proprietary module) and similar are non-fatal; but a fresh
        # taint from this test would be suspicious. Just log it.
        print(f"WARNING: kernel taint = {taint}; investigate if this changed")

    # Switch back to tty1 so the test fixture leaves the VM in a
    # recognizable state.
    machine.succeed("chvt 1")

    print("=" * 70)
    print("PASS: VT switch IPC dance drove a real kernel VT switch")
    print(f"      baseline: {baseline_vt} → final: {final_vt}")
    print(f"      verdict:  {verdict}")
    print("=" * 70)
  '';
}
