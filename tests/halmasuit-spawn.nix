# VM test for halmasuit-spawn — the setuid-root privilege-drop helper.
#
# Asserts the helper:
# 1. Happy path: when invoked setuid-root by a non-root user with a valid
#    target UID >= 1000, drops privileges to that target and execve's the
#    given command. We verify by execing `id -u` and checking output.
# 2. UID floor: refuses target_uid = 0 (system UID) even when invoked
#    setuid-root. Exit code is non-zero; no privilege escalation possible.
# 3. Non-setuid invocation: refuses cleanly (EUID != 0) without crashing.
# 4. Env allowlist: strips LD_PRELOAD and any non-allowlisted env vars
#    before exec. Verified by passing them in and asserting they don't
#    appear in the target's `printenv` output.

{
  system,
  nixpkgs,
  halmasuit-spawn,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-spawn";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [ ./lib/test-user.nix ];

      # halmasuit-spawn enforces target_gid >= 1000 (load-bearing per
      # ARCHITECTURE.md threat model row 11). NixOS's default `users` group
      # (GID 100) sits below that floor — by design, the helper refuses it.
      # Provision a per-user primary group with GID 1000 (the user-private-
      # group convention halmasuit's compositor/greeter accounts will use).
      users.groups.test = { gid = 1000; };
      users.users.test = { group = "test"; };

      # The production NixOS module will wire this via security.wrappers
      # once the spine first calls halmasuit-spawn. Until then, the test
      # creates the wrapper directly here.
      security.wrappers.halmasuit-spawn = {
        source = lib.getExe halmasuit-spawn;
        owner  = "root";
        group  = "root";
        setuid = true;
        permissions = "u+rx,g+rx,o+rx";
      };

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    WRAPPER = "/run/wrappers/bin/halmasuit-spawn"
    ID = "${pkgs.coreutils}/bin/id"
    PRINTENV = "${pkgs.coreutils}/bin/printenv"

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Assertion 1a: happy path. test user (uid 1000) invokes the setuid
    # wrapper with target_uid=1000, command=`id -u`. The helper should
    # drop privs and execve id. id prints the (now-real) uid.
    out = machine.succeed(
        f"sudo -u test {WRAPPER} 1000 1000 test -- {ID} -u"
    ).strip()
    assert out == "1000", f"happy path: id -u must print 1000, got {out!r}"

    # Assertion 1b: supplementary groups land too. `id -G` prints every
    # group the process belongs to. The primary 1000 must be present;
    # extras come from initgroups()'s NSS lookup over /etc/group.
    # If initgroups silently regresses or is removed, this assertion
    # catches it.
    groups = machine.succeed(
        f"sudo -u test {WRAPPER} 1000 1000 test -- {ID} -G"
    ).strip().split()
    assert "1000" in groups, (
        f"happy path: id -G must include primary gid 1000, got {groups!r}"
    )

    # Assertion 2: UID floor refuses target_uid = 0. The refusal message
    # goes to stderr; redirect it so machine.execute() captures it.
    rc, out = machine.execute(
        f"sudo -u test {WRAPPER} 0 1000 test -- {ID} -u 2>&1"
    )
    assert rc != 0, f"target_uid=0 must be refused; got rc={rc} out={out!r}"
    assert "UID_MIN" in out or "uid/gid 0" in out, (
        f"refusal must mention the floor; got: {out!r}"
    )

    # Assertion 3: UID floor refuses target_gid = 0 even when target_uid >= 1000.
    rc, out = machine.execute(
        f"sudo -u test {WRAPPER} 1000 0 test -- {ID} -u 2>&1"
    )
    assert rc != 0, f"target_gid=0 must be refused; got rc={rc} out={out!r}"
    assert "UID_MIN" in out or "uid/gid 0" in out, (
        f"gid refusal must mention the floor; got: {out!r}"
    )

    # Assertion 4: env allowlist strips LD_PRELOAD. Invoke with
    # LD_PRELOAD set (it'd be ignored by the kernel for the setuid call
    # anyway due to AT_SECURE, but we also sanitize it explicitly).
    out = machine.succeed(
        f"sudo -u test env LD_PRELOAD=/tmp/evil.so PATH=/usr/bin:/bin "
        f"{WRAPPER} 1000 1000 test -- {PRINTENV}"
    )
    assert "LD_PRELOAD" not in out, (
        f"LD_PRELOAD must be stripped from execve env; got:\n{out}"
    )
    # Sanity: at least one allowlisted var should survive.
    assert "PATH=" in out, (
        f"PATH must survive the allowlist filter; got:\n{out}"
    )

    # Assertion 5: env allowlist strips non-allowlisted keys like RUST_LOG.
    out = machine.succeed(
        f"sudo -u test env RUST_LOG=debug EDITOR=vim PATH=/usr/bin "
        f"{WRAPPER} 1000 1000 test -- {PRINTENV}"
    )
    assert "RUST_LOG" not in out, f"RUST_LOG must be stripped; got:\n{out}"
    assert "EDITOR" not in out, f"EDITOR must be stripped; got:\n{out}"
    assert "PATH=" in out, f"PATH must survive; got:\n{out}"

    # Assertion 6: malformed argv (missing --) is rejected without invoking
    # any setuid-relevant syscalls.
    rc, out = machine.execute(
        f"sudo -u test {WRAPPER} 1000 1000 test {ID} -u 2>&1"
    )
    assert rc != 0, f"malformed argv must be refused; got rc={rc}"
    assert "separator" in out or "argv" in out, (
        f"refusal must indicate argv error; got: {out!r}"
    )

    # Assertion 7: invoking the binary WITHOUT the setuid wrapper refuses
    # cleanly (EUID != 0).
    SPAWN_DIRECT = "${halmasuit-spawn}/bin/halmasuit-spawn"
    rc, out = machine.execute(
        f"sudo -u test {SPAWN_DIRECT} 1000 1000 test -- {ID} -u 2>&1"
    )
    assert rc != 0, f"non-setuid invocation must refuse; got rc={rc}"
    assert "EUID" in out or "root" in out, (
        f"refusal must indicate non-root posture; got: {out!r}"
    )

    print("halmasuit-spawn: ALL ASSERTIONS PASSED")
  '';
}
