# Epic #71 R1.4 — broker master-drop timeout invariant (systemd #21388).
#
# Load-bearing regression gate: the broker MUST FAIL the VT-switch
# request when the compositor does not ack `VtSwitchMasterDropped`
# within the 5s watchdog window. It MUST NOT fire `VT_ACTIVATE` to
# "make the request go through anyway."
#
# This is the systemd #21388 bug class encoded at the kernel-state
# level. The unit test
# `crates/halmasuit-session/src/broker.rs::tests::
# vt_switch_master_drop_timeout_never_fires_activate` pins this
# property at the protocol layer via a mocked VT_ACTIVATE. This VM
# test pins it at the LIVE kernel layer: a broker that incorrectly
# fires VT_ACTIVATE on timeout would change
# /sys/class/tty/tty0/active even though the protocol says Rejected.
#
# Mechanism:
#   * Broker deployed identically to vt-switch-roundtrip.
#   * Test client launched in `--mode timeout`: receives
#     VtSwitchPrepare(fd) but DELIBERATELY never sends the
#     MasterDropped ack.
#   * Broker's 5s watchdog should fire, emit
#     VtSwitchRejected{MasterDropTimeout}, NOT call VT_ACTIVATE.
#   * Test script asserts:
#       1. `VERDICT: REJECTED reason=MasterDropTimeout` appears
#          in the client log.
#       2. `/sys/class/tty/tty0/active` is UNCHANGED (still tty1).
#          THIS IS THE LOAD-BEARING ASSERTION — anything else means
#          the broker fired VT_ACTIVATE despite timing out, which
#          is the bug this gate exists to prevent.

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
  name = "vt-switch-master-drop-timeout";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
      ];

      services.halmasuit.session = {
        enable  = true;
        package = halmasuit-session;
      };

      # See tests/vt-switch-roundtrip.nix for why we declare the
      # greeter user/group manually in a broker-only test setup.
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

    baseline_vt = machine.succeed("cat /sys/class/tty/tty0/active").strip()
    print(f"baseline active VT: {baseline_vt}")
    if baseline_vt != "tty1":
        raise AssertionError(
            f"VM baseline should be tty1, got {baseline_vt}; test logic depends on this."
        )

    machine.succeed(
        "systemd-run --unit=vt-test-client --collect "
        "--uid=999 --gid=999 "
        "--property=Type=simple "
        "--property=StandardOutput=journal "
        "--property=StandardError=journal "
        "${halmasuit-vt-test-client}/bin/halmasuit-vt-test-client "
        "--broker-socket /run/halmasuit-session.sock "
        "--target-vt 2 "
        "--mode timeout "
        "--log /tmp/vt-test.log"
    )

    # The broker's master-drop watchdog is 5s. Poll up to 15s for a
    # VERDICT line (10s margin for VM jitter + the broker's reply
    # send + the log fsync).
    import time
    deadline = time.time() + 15
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
        raise AssertionError(
            "client did not log a VERDICT within 15s. Broker's watchdog "
            "should fire at ~5s; investigate."
        )

    print(f"client verdict: {verdict}")
    if verdict != "VERDICT: REJECTED reason=MasterDropTimeout":
        dump()
        raise AssertionError(
            f"expected 'VERDICT: REJECTED reason=MasterDropTimeout', got '{verdict}'. "
            f"This is the systemd #21388 invariant: broker MUST emit Rejected "
            f"with reason=MasterDropTimeout on the 5s watchdog, NOT silently proceed."
        )

    # ────────────────────────────────────────────────────────────────
    # THE LOAD-BEARING ASSERTION
    # ────────────────────────────────────────────────────────────────
    # If the broker (incorrectly) fired VT_ACTIVATE despite timing out
    # on the master-drop ack, the kernel WOULD switch VTs and we'd see
    # /sys/class/tty/tty0/active change. The whole point of this
    # gate is that the broker MUST NOT fire VT_ACTIVATE here.
    final_vt = machine.succeed("cat /sys/class/tty/tty0/active").strip()
    print(f"final active VT: {final_vt}")
    if final_vt != baseline_vt:
        dump()
        raise AssertionError(
            f"LOAD-BEARING FAILURE: kernel VT changed from {baseline_vt} to "
            f"{final_vt} despite the broker emitting VtSwitchRejected"
            f"{{MasterDropTimeout}}. This means the broker fired VT_ACTIVATE "
            f"on the timeout path — the systemd #21388 bug class.\n"
            f"\n"
            f"This regression gate exists to catch exactly this bug. "
            f"Investigate `serve_vt_switch_request` in "
            f"crates/halmasuit-session/src/broker.rs — the timeout path "
            f"MUST `return` without calling `vt_activate()`."
        )

    # Verify the broker's journal has the systemd #21388 marker so
    # we know the watchdog actually fired (vs. some unrelated path
    # producing the same end-state). The exact log text comes from
    # crates/halmasuit-session/src/broker.rs:
    #   "VtSwitchMasterDropped did not arrive within {VT_MASTER_DROP_TIMEOUT:?}; "
    #   "failing the request (systemd #21388: NOT firing VT_ACTIVATE on timeout)"
    broker_log = machine.succeed("journalctl -u halmasuit-session --no-pager")
    if "NOT firing VT_ACTIVATE" not in broker_log:
        print("=== broker journal (for diagnosis) ===")
        print(broker_log)
        raise AssertionError(
            "broker journal does NOT contain the watchdog trip marker "
            "('NOT firing VT_ACTIVATE on timeout'). The Rejected reply "
            "might have come from a different code path than the watchdog. "
            "This test's invariant is specifically about the watchdog path; "
            "if the broker reaches Rejected via another route, that's also "
            "broken (the master-drop ack didn't arrive)."
        )

    print("=" * 70)
    print("PASS: broker FAILED the VT switch on master-drop timeout")
    print(f"      VT remained {baseline_vt} (NOT changed to tty2)")
    print(f"      verdict:  {verdict}")
    print("      broker journal contains the 'NOT firing VT_ACTIVATE' marker")
    print("=" * 70)
  '';
}
