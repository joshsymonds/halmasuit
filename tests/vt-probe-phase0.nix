# halmasuit-vt-probe Phase 0: validate that an unprivileged process can
# satisfy the kernel's `perm` check for VT_RELDISP on an inherited VT fd
# by calling TIOCSCTTY on it — without holding CAP_SYS_TTY_CONFIG.
#
# Empirical question (per drivers/tty/vt/vt_ioctl.c):
#   if (current->signal->tty == tty || capable(CAP_SYS_TTY_CONFIG))
#       perm = 1;
# If TIOCSCTTY makes the inherited fd the calling process's controlling
# TTY, the first arm is satisfied and the unprivileged process can call
# VT_RELDISP without the cap. That's the load-bearing assumption of
# Epic #71's broker-passes-fd design.
#
# Mechanism:
#   * Systemd unit invokes halmasuit-vt-probe as root with --vt /dev/tty2.
#   * The probe opens /dev/tty2, sets VT_SETMODE PROCESS with SIGUSR1/2,
#     forks, drops privileges in the child to halmasuit-compositor (uid
#     998), empties caps, setsid()s, TIOCSCTTYs the inherited fd.
#   * testScript triggers `chvt 1` from the test driver, switching away
#     from tty2. Kernel sends SIGUSR1 to the probe (VT_PROCESS owner of
#     tty2). Probe calls VT_RELDISP(1) and logs the result.
#   * testScript reads /tmp/vt-probe.log and asserts a verdict line.
#
# Outcomes:
#   - "VERDICT: PASS" → broker hands off fd; compositor handles RELDISP.
#     The Epic #71 design's primary path.
#   - "VERDICT: FAIL" (TIOCSCTTY or VT_RELDISP returned EPERM/EACCES) →
#     design must use the fallback path: broker also brokers VT_RELDISP.
#     Not a test failure per se; the probe answered the question.
#
# What this probe does NOT validate:
#   - The full broker-mediated production protocol (SCM_RIGHTS fd-pass
#     over a long-lived Unix socket, broker watchdog timer, drop-master
#     ordering enforcement). All of that lands in Epic #71 R-series
#     tasks; this probe just answers the kernel-API gating question.

{
  system,
  nixpkgs,
}:

let
  pkgs = import nixpkgs {
    inherit system;
  };

  halmasuitVtProbe = pkgs.rustPlatform.buildRustPackage {
    pname   = "halmasuit-vt-probe";
    version = "0.1.0";
    src     = ../.;
    cargoLock = {
      lockFile = ../Cargo.lock;
      allowBuiltinFetchGit = true;
    };
    cargoBuildFlags = [ "-p" "halmasuit-vt-probe" ];
    doCheck = false; # NixOS VM test is the actual test
    meta = {
      description = "Phase 0 research probe — TIOCSCTTY + VT_RELDISP without CAP_SYS_TTY_CONFIG (Epic #71 VT switching)";
      license     = pkgs.lib.licenses.asl20;
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "vt-probe-phase0";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
      };

      # Compositor-user account mirroring halmasuit's deployment.
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-compositor";
      };
      users.groups.halmasuit-compositor.gid = 998;

      environment.systemPackages = [ halmasuitVtProbe pkgs.kbd ];

      # The probe is a one-shot binary; we launch it from the test
      # script directly via `systemd-run` so we can hand-control
      # timing rather than racing a wantedBy=multi-user.target unit.
      # No persistent unit needed.
    };

  testScript = ''
    def dump_log(label):
        """Best-effort log dump, doesn't fail if the file doesn't exist."""
        print(f"=== probe log ({label}) ===")
        out = machine.succeed("cat /tmp/vt-probe.log 2>/dev/null || echo '(no log file yet)'")
        print(out)
        print("=" * (16 + len(label)))
        return out

    def dump_journal():
        print("=== probe journal ===")
        print(machine.succeed("journalctl -u vt-probe-phase0 --no-pager 2>&1 || true"))
        print("======================")

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Launch the probe as a transient systemd-run unit so it inherits
    # systemd's TTY-less environment (matches how the production
    # broker will spawn it — no controlling TTY inherited). The unit
    # runs as root; the probe drops privileges in-process.
    machine.succeed(
        "systemd-run --unit=vt-probe-phase0 --collect "
        "--property=Type=simple "
        "--property=StandardOutput=journal "
        "--property=StandardError=journal "
        "${halmasuitVtProbe}/bin/halmasuit-vt-probe "
        "--vt /dev/tty2 "
        "--target-user halmasuit-compositor "
        "--log /tmp/vt-probe.log"
    )

    # Poll until the probe either reaches the WAITING state (success
    # path), logs TIOCSCTTY: FAILED (the design's negative verdict),
    # or the systemd unit exits (early failure / probe bug). Distinguish
    # by checking the unit's ActiveState and the log content.
    import time
    deadline = time.time() + 25
    last_log = ""
    while time.time() < deadline:
        last_log = machine.succeed("cat /tmp/vt-probe.log 2>/dev/null || echo")
        if "WAITING:" in last_log:
            print("Probe reached WAITING state")
            break
        if "TIOCSCTTY: FAILED" in last_log or "VT_SETMODE: FAILED" in last_log:
            print("Probe logged FAILED before WAITING")
            break
        # Has the unit already exited?
        active_state = machine.succeed(
            "systemctl show -p ActiveState --value vt-probe-phase0 2>&1 || echo unknown"
        ).strip()
        if active_state in ("inactive", "failed"):
            dump_log("after-early-exit")
            dump_journal()
            raise AssertionError(
                f"Probe unit exited (ActiveState={active_state}) before reaching "
                f"WAITING or logging a verdict. See logs above for the diagnosis "
                f"point. Note: ANY clean exit before VT_RELDISP is a probe defect, "
                f"not a design verdict — the kernel question is unanswered."
            )
        time.sleep(0.5)
    else:
        dump_log("after-poll-timeout")
        dump_journal()
        raise AssertionError("Probe never reached a verdict within 25s")

    log_so_far = dump_log("after-TIOCSCTTY")

    if "TIOCSCTTY: FAILED" in log_so_far:
        raise AssertionError(
            "TIOCSCTTY failed without CAP_SYS_TTY_CONFIG. "
            "Design verdict: broker-passes-fd model not viable; "
            "Epic #71 must use the fallback path (broker also brokers "
            "TIOCSCTTY-equivalent setup OR keeps the controlling-TTY "
            "relationship in the broker)."
        )

    # TIOCSCTTY + VT_SETMODE succeeded. Now trigger SIGUSR1 to the probe.
    # The kernel only sends SIGUSR1 (the VT_PROCESS relsig) when
    # switching AWAY from the probe's VT. NixOS test VM boots on tty1
    # as the active VT, so a `chvt 1` from here is a no-op — there's
    # no "switch away from tty2" because tty2 isn't active.
    #
    # First switch TO tty2 (kernel sends SIGUSR2/acqsig, probe acks
    # with VT_RELDISP(VT_ACKACQ)), then AWAY to tty1 (kernel sends
    # SIGUSR1/relsig, probe acks with VT_RELDISP(1)). The probe
    # exercises BOTH VT_RELDISP arg variants — both unprivileged.
    machine.succeed("chvt 2")
    time.sleep(0.5)
    machine.succeed("chvt 1")

    # Wait for VERDICT.
    deadline = time.time() + 25
    while time.time() < deadline:
        contents = machine.succeed("cat /tmp/vt-probe.log")
        if "VERDICT:" in contents:
            break
        time.sleep(0.5)
    else:
        dump_log("verdict-timeout")
        dump_journal()
        raise AssertionError("Probe did not log a VERDICT within 25s after chvt 1")

    log_final = dump_log("after-chvt")

    if "VERDICT: PASS" in log_final:
        print("=" * 70)
        print("PASS: TIOCSCTTY + VT_RELDISP works without CAP_SYS_TTY_CONFIG.")
        print("Epic #71 design path validated: broker hands off fd via")
        print("SCM_RIGHTS; the compositor handles VT_RELDISP itself.")
        print("=" * 70)
    elif "VERDICT: FAIL" in log_final:
        raise AssertionError(
            "VT_RELDISP failed without CAP_SYS_TTY_CONFIG. "
            "Design verdict: Epic #71 must use fallback path (broker "
            "also brokers VT_RELDISP)."
        )
    else:
        raise AssertionError("Probe did not reach a recognizable verdict")
  '';
}
