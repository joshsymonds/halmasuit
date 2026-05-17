# Epic #1 FLAGSHIP gate (epic headline) — the single `pam_handle_t`
# demonstrably spans auth → session, proven with REAL pam_mount.
#
# Drives the deployed `halmasuit-session` broker through the COMPLETE
# one-handle lifecycle against the REAL libpam stack with the REAL
# test user (NO mock, NO PAM bypass, NO hand-rolled fake module —
# CLAUDE.md hard rule): a Python greeter speaks the framed wire codec,
# answers the real pam_unix conversation with the real password,
# receives `success`, sends `start_session`, and the
# privilege-dropped session leader runs.
#
# THE one-handle proof is real upstream `pam_mount` with an ENCRYPTED
# (LUKS) volume whose passphrase IS the login password — the exact
# §0.2 channel: pam_mount's AUTH module captures the password into the
# pam handle (`pam_set_data`, process-local heap); its SESSION module
# reads it back to `cryptsetup`-open and mount the volume at
# `pam_open_session`. This works ONLY if ONE handle in ONE process
# spans auth→session. A split-handle / two-process design (the
# anti-pattern §0.2 rejects) leaves the session pam_mount with no
# stored secret → the encrypted $HOME marker is silently absent, no
# error — the precise failure this epic exists to prevent, made into
# a RED gate by an explicit marker assertion.
#
# Folded in as cheap additional signals on the same successful
# lifecycle (no second slow gate):
#  - Amendment A1.3: a session-phase `pam_env` sets the allowlisted
#    `LANG` to a sentinel; it reaches the leader's env ONLY via
#    `pam_getenvlist`-merge (a blind env replace would drop it).
#  - R7/R11 getgrouplist-MERGE: the broker process carries the
#    supplementary group `shadow` (the unit's `SupplementaryGroups=`),
#    which is NOT a /etc/group membership of `test`. It reaches the
#    privilege-dropped leader ONLY if `merged_groups` UNIONed the
#    broker's established supplementary set; a blind `initgroups(test)`
#    would yield only test's static groups and DROP `shadow` — the
#    exact anti-pattern R7/R11 forbids.
#  - R8 (identity is PAM-resolved test/1000/1000) and R6 (clean
#    teardown → idle-exit, no standing root).
#
# State-based throughout (`wait_until_succeeds`/`fail`), never
# `time.sleep` (memory feedback-state-based-polling). Diagnostics
# (client.out + broker journal + /tmp/oh) are dumped unconditionally
# before the assertions so any failure is self-explaining.

{
  system,
  nixpkgs,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  co = "${pkgs.coreutils}/bin";

  # The session program the broker fork-drops to as the PAM-resolved
  # uid (Epic R7). Absolute coreutils — the leader's env is the
  # pam_getenvlist+allowlist set and need not contain PATH.
  leader = pkgs.writeShellScript "oh-leader.sh" ''
    set -u
    ${co}/mkdir -p /tmp/oh
    ${co}/id > /tmp/oh/leader-id 2>&1
    ${co}/env > /tmp/oh/leader-env 2>&1
    # The one-handle proof: this file exists with the secret ONLY if
    # real pam_mount decrypted+mounted the LUKS volume at session,
    # using the auth-phase password from the SAME pam handle.
    ${co}/cat /run/oh-secure/marker > /tmp/oh/leader-secret 2>&1 || \
      ${co}/echo "NO_SECRET (encrypted volume not mounted)" > /tmp/oh/leader-secret
    ${co}/echo done > /tmp/oh/leader-done
  '';

  # session-phase pam_env: set the ALLOWLISTED `LANG` to a sentinel
  # (allowlisted → survives the R11 env sanitizer; distinctive →
  # unambiguous). Amendment A1.3.
  pamEnvConf = pkgs.writeText "oh-pam-env.conf" ''
    LANG DEFAULT=oh_ONEHANDLE.UTF-8
  '';

  # pam_mount config — mirrors the NixOS security.pam.mount template
  # (helpers by absolute store path; PATH for mount.crypt → cryptsetup
  # + util-linux). One crypt volume for `test`; its key is the login
  # password pam_mount captured at auth.
  pamMountConf = pkgs.writeText "oh-pam_mount.conf.xml" ''
    <?xml version="1.0" encoding="utf-8" ?>
    <!DOCTYPE pam_mount SYSTEM "pam_mount.conf.xml.dtd">
    <pam_mount>
    <debug enable="1" />
    <logout wait="0" hup="no" term="no" kill="no" />
    <path>${pkgs.lib.makeBinPath [ pkgs.util-linux pkgs.cryptsetup ]}</path>
    <mkmountpoint enable="1" remove="false" />
    <fusemount>${pkgs.fuse}/bin/mount.fuse %(VOLUME) %(MNTPT) -o ,%(OPTIONS)'</fusemount>
    <fuseumount>${pkgs.fuse}/bin/fusermount -u %(MNTPT)</fuseumount>
    <cryptmount>${pkgs.pam_mount}/bin/mount.crypt -o , %(VOLUME) %(MNTPT)</cryptmount>
    <cryptumount>${pkgs.pam_mount}/bin/umount.crypt %(MNTPT)</cryptumount>
    <pmvarrun>${pkgs.pam_mount}/bin/pmvarrun -u %(USER) -o %(OPERATION)</pmvarrun>
    <volume user="test" fstype="crypt" path="/var/lib/oh/secret.img" mountpoint="/run/oh-secure" />
    </pam_mount>
  '';

  # Full-handshake greeter: begin_auth → answer every conv prompt with
  # the real password → on success send start_session → drain to EOF.
  client = pkgs.writeText "oh-client.py" ''
    import socket, struct, sys, json
    PATH = "/run/halmasuit-session.sock"
    LEADER = sys.argv[1]
    def frame(obj):
        b = json.dumps(obj, separators=(",", ":")).encode()
        return struct.pack("=I", len(b)) + b
    def recv1(s):
        d = s.recv(1 << 20)
        if not d:
            return None
        ln = struct.unpack("=I", d[:4])[0]
        return json.loads(d[4:4 + ln])
    s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    s.connect(PATH)
    s.send(frame({"type": "begin_auth",
                  "service": "halmasuit-onehandle",
                  "username": "test"}))
    while True:
        m = recv1(s)
        if m is None:
            print("SESSION_END", flush=True); break
        t = m.get("type", "?")
        if t == "conv_prompt":
            s.send(frame({"type": "conv_response", "response": "test"}))
        elif t == "success":
            print("SUCCESS user=%s uid=%s gid=%s"
                  % (m["username"], m["uid"], m["gid"]), flush=True)
            s.send(frame({"type": "start_session",
                          "cmd": [LEADER], "env": []}))
        elif t == "failure":
            print("FAILURE " + m.get("reason", "?"), flush=True); break
        else:
            print("FRAME " + t, flush=True)
  '';
in
pkgs.testers.runNixOSTest {
  name = "session-onehandle";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      # Epic R8: resolved gid ≥ the broker UID floor (1000).
      users.groups.test = { gid = 1000; };
      users.users.test = { group = "test"; };

      # Our pam_mount.conf.xml at the path pam_mount.so reads by
      # default. We deliberately do NOT enable security.pam.mount
      # (its per-service injection targets structured pam services,
      # not our absolute-path `.text` service).
      environment.etc."security/pam_mount.conf.xml".source = pamMountConf;

      # REAL PAM stack — modules by absolute store path. NO mock.
      #  auth:    pam_unix (password)
      #           + pam_mount (captures the password into the handle)
      #  session: pam_env (sentinel LANG) + pam_mount (reads the
      #           captured password, cryptsetup-opens + mounts the
      #           LUKS volume — the one-handle proof)
      security.pam.services.halmasuit-onehandle.text = ''
        auth     required ${pkgs.pam}/lib/security/pam_unix.so
        auth     optional ${pkgs.pam_mount}/lib/security/pam_mount.so
        account  required ${pkgs.pam}/lib/security/pam_unix.so
        session  required ${pkgs.pam}/lib/security/pam_env.so conffile=${pamEnvConf}
        session  optional ${pkgs.pam_mount}/lib/security/pam_mount.so
      '';

      services.halmasuit.session = {
        enable  = true;
        package = halmasuit-session;
      };
      services.halmasuit.greeterUid = 1000;
      services.halmasuit.pamService = "halmasuit-onehandle";

      environment.systemPackages = [
        pkgs.python3
        pkgs.cryptsetup
        pkgs.e2fsprogs
        pkgs.util-linux
      ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 2048;
      };
    };

  testScript = ''
    machine.wait_for_unit("sockets.target")
    machine.wait_until_succeeds("systemctl is-active halmasuit-session.socket")
    machine.fail("pgrep -x halmasuit-session")

    # Build the LUKS container at RUNTIME (no build-time crypto in the
    # sandbox). Its passphrase IS the test user's login password
    # ("test"); a marker lives inside the encrypted filesystem.
    machine.succeed("mkdir -p /var/lib/oh /run/oh-secure /tmp/oh && chmod 0777 /tmp/oh")
    machine.succeed("dd if=/dev/zero of=/var/lib/oh/secret.img bs=1M count=48")
    machine.succeed(
        "echo -n test | cryptsetup luksFormat --batch-mode --type luks2 "
        "/var/lib/oh/secret.img -"
    )
    machine.succeed(
        "echo -n test | cryptsetup open --type luks2 "
        "/var/lib/oh/secret.img ohluks -"
    )
    machine.succeed("mkfs.ext4 -q /dev/mapper/ohluks")
    machine.succeed(
        "mkdir -p /mnt/x && mount /dev/mapper/ohluks /mnt/x && "
        "echo ONEHANDLE_SECRET > /mnt/x/marker && chmod 0644 /mnt/x/marker && "
        "umount /mnt/x && cryptsetup close ohluks"
    )

    # Full one-handle lifecycle, driven as the greeter uid (1000).
    machine.succeed(
        "runuser -u test -- sh -c "
        "'python3 ${client} ${leader} > /tmp/oh/client.out 2>&1 &'"
    )

    # R8: PAM-resolved identity relayed to the greeter.
    machine.wait_until_succeeds(
        "grep -q 'SUCCESS user=test uid=1000 gid=1000' /tmp/oh/client.out"
    )

    # Wait for the lifecycle to CONCLUDE, then dump evidence
    # unconditionally (diagnostics-before-assertions).
    machine.wait_until_succeeds(
        "grep -qE 'SESSION_END|FAILURE' /tmp/oh/client.out", timeout=90
    )
    print("=== client.out ===")
    print(machine.execute("cat /tmp/oh/client.out")[1])
    print("=== journalctl -u halmasuit-session ===")
    print(machine.execute(
        "journalctl -u halmasuit-session --no-pager -o cat"
    )[1])
    print("=== /tmp/oh ===")
    print(machine.execute(
        "ls -la /tmp/oh; for f in leader-id leader-env leader-secret; do "
        "echo \"--- $f ---\"; cat /tmp/oh/$f 2>&1; done"
    )[1])

    # The session leader ran (fork-then-drop, R7).
    machine.wait_until_succeeds("test -f /tmp/oh/leader-done", timeout=30)
    machine.succeed("grep -q 'uid=1000' /tmp/oh/leader-id")

    # ── THE one-handle proof: real pam_mount decrypted+mounted the
    #    LUKS volume at session using the auth-phase password from the
    #    SAME handle. A split handle → no stored secret → marker
    #    silently absent (the §0.2 failure) → this assertion is RED.
    machine.succeed("grep -q '^ONEHANDLE_SECRET$' /tmp/oh/leader-secret")

    # ── Amendment A1.3: session-phase pam_env sentinel reached the
    #    leader env via pam_getenvlist-merge (greeter sent env=[]).
    machine.succeed("grep -q '^LANG=oh_ONEHANDLE.UTF-8$' /tmp/oh/leader-env")

    # ── R7/R11 getgrouplist-MERGE: `shadow` (the broker unit's
    #    SupplementaryGroups=, NOT a /etc/group membership of `test`)
    #    reached the privilege-dropped leader → merged_groups UNIONed
    #    the broker's established supplementary set. A blind
    #    initgroups(test) would yield only test/wheel/video/input and
    #    DROP `shadow` — the exact anti-pattern R7/R11 forbids.
    machine.succeed("grep -qw shadow /tmp/oh/leader-id")

    # R6: leader exited → close_session/teardown → broker idle-exits,
    # unit deactivates, no standing root.
    machine.wait_until_succeeds(
        '[ "$(systemctl is-active halmasuit-session.service)" != active ]',
        timeout=120,
    )
    machine.fail("pgrep -x halmasuit-session")

    print(
        "session-onehandle: ONE pam_handle_t spanned auth→session in "
        "the privileged broker — REAL pam_mount decrypted+mounted a "
        "LUKS volume at session using the auth-phase password from the "
        "SAME handle (the §0.2 channel; a split handle would silently "
        "fail), the broker's established `shadow` group MERGED into "
        "the privilege-dropped leader (blind initgroups would drop "
        "it), the session-phase pam_env var survived into the "
        "leader's environment (Amendment A1.3); identity was "
        "PAM-resolved test/1000/1000 (R8); clean teardown → no "
        "standing root (R6)."
    )
  '';
}
