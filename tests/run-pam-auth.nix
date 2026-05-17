# Epic #1 R12 + R4 — the real-PAM gate.
#
# Proves halmasuit-session authenticates against the REAL libpam stack
# with the REAL test user, end-to-end over a real SOCK_SEQPACKET
# socketpair, in BOTH paths: in-process `run_pam_auth` and through the
# ephemeral SIGKILL-able privileged fork `spawn_auth_worker` (R4). NO
# mock, NO PAM bypass (CLAUDE.md hard rule). The driver calls the real
# code; the testScript only inspects its parseable output.
#
# Asserts:
#   1. Correct password → identity whose username/uid/gid come from
#      PAM's resolved name's pwent (Epic R8): test/1000/1000 — both
#      in-process and via the fork.
#   2. Wrong password → fail closed (non-zero, bounded by the driver's
#      watchdog, no hang, no panic) — both paths.
#   3. Via the fork: the worker child is reaped, no orphan survives.

{
  system,
  nixpkgs,
  halmasuit-session-pam-testdriver,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "run-pam-auth";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [ ./lib/test-user.nix ];

      # Epic R8: the resolved gid must be >= the UID floor the broker
      # enforces. NixOS's default `users` group is GID 100 (< 1000);
      # give `test` a user-private group at GID 1000 (same convention
      # as halmasuit-spawn.nix).
      users.groups.test = { gid = 1000; };
      users.users.test = { group = "test"; };

      # A minimal REAL PAM service: pam_unix only. Module referenced by
      # absolute store path so resolution never depends on PAM's search
      # path. This is real authentication — not a mock.
      security.pam.services.halmasuit-pam-test.text = ''
        auth     required ${pkgs.pam}/lib/security/pam_unix.so
        account  required ${pkgs.pam}/lib/security/pam_unix.so
      '';

      environment.systemPackages = [ halmasuit-session-pam-testdriver ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    # Correct password (test user's password is "test", per
    # tests/lib/test-user.nix). Driver runs as root so pam_unix can
    # read /etc/shadow directly (the broker is privileged by design).
    machine.succeed("printf 'test' > /tmp/pw")
    out = machine.succeed(
        "halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw"
    )
    assert "OK user=test uid=1000 gid=1000" in out, (
        f"expected PAM-resolved identity test/1000/1000, got: {out!r}"
    )

    # Wrong password → fail closed (non-zero), bounded, no hang/panic.
    machine.succeed("printf 'definitely-not-the-password' > /tmp/bad")
    machine.fail(
        "halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/bad"
    )

    # ── Epic R4: REAL PAM through the ephemeral privileged fork ──
    # Same assertions, but PAM now runs in spawn_auth_worker's
    # disposable child; this driver process is the broker parent
    # relaying the conversation and reaping the child.
    out = machine.succeed(
        "halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw --via-fork"
    )
    assert "OK user=test uid=1000 gid=1000" in out, (
        f"via-fork: expected test/1000/1000, got: {out!r}"
    )
    # (The driver reaps the worker via handle.wait() before exiting;
    # the authoritative no-orphan proof is the pgrep check below.)

    # Wrong password through the fork → fail closed.
    machine.fail(
        "halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/bad --via-fork"
    )

    # No orphaned worker process survives a via-fork run.
    machine.succeed(
        "halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw --via-fork"
    )
    machine.fail("pgrep -f halmasuit-session-pam-testdriver")

    print(
        "run-pam-auth: REAL pam_unix succeeded for test (uid=1000 "
        "gid=1000, identity from pam_get_user) BOTH in-process and "
        "through the ephemeral privileged fork (R4); wrong password "
        "rejected fail-closed; worker child reaped, no orphan."
    )
  '';
}
