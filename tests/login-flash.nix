# login-flash: the v2 gate.
#
# Asserts the compositor's PID is continuous across the
# greeter→session boundary — i.e. halmasuit doesn't restart when a
# user logs in. Same intent as the v1 baseline; under v2's
# architecture the long-lived compositor is halmasuit (not niri), so
# the PID we measure is halmasuit's.
#
# Auth driver is a deterministic Python script spoken over halmasuit's
# greetd wire protocol. It runs immediately when halmasuit fork+execs
# the greeter — no file-handshake gate; the test driver synchronizes
# off journald events instead, which sidesteps the hardened unit's
# /tmp being read-only.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-spawn,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # Auth driver. Connects to halmasuit's greetd socket (via the
  # GREETD_SOCK env halmasuit sets for its greeter children), drives
  # auth + start_session for user `test`, then idles so halmasuit's
  # greeter slot stays occupied while the test driver inspects state.
  testGreetdAuth = pkgs.writeScript "test-greetd-auth.py" ''
    #!${pkgs.python3}/bin/python3
    import json, os, socket, struct, sys, time

    sock_path = os.environ.get("GREETD_SOCK")
    if not sock_path:
        print("GREETD_SOCK not set in greeter env", file=sys.stderr)
        sys.exit(1)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(sock_path)

    def send(msg):
        body = json.dumps(msg).encode()
        sock.sendall(struct.pack("=I", len(body)) + body)

    def recv():
        hdr = b""
        while len(hdr) < 4:
            chunk = sock.recv(4 - len(hdr))
            if not chunk:
                raise RuntimeError("greetd socket closed")
            hdr += chunk
        n = struct.unpack("=I", hdr)[0]
        body = b""
        while len(body) < n:
            chunk = sock.recv(n - len(body))
            if not chunk:
                raise RuntimeError("greetd socket closed mid-body")
            body += chunk
        return json.loads(body.decode())

    print("test-greeter: create_session for user=test", file=sys.stderr)
    send({"type": "create_session", "username": "test"})

    while True:
        r = recv()
        print(f"test-greeter: recv {r}", file=sys.stderr)
        kind = r.get("type")
        if kind == "auth_message":
            send({"type": "post_auth_message_response", "response": "test"})
        elif kind == "success":
            break
        elif kind == "error":
            print(f"test-greeter: auth error: {r}", file=sys.stderr)
            sys.exit(1)
        else:
            print(f"test-greeter: unexpected {r}", file=sys.stderr)
            sys.exit(1)

    # Session command: `sleep infinity` as the test user. The
    # architectural assertion (halmasuit's PID is continuous) doesn't
    # depend on what the session command IS — just that halmasuit-spawn
    # launches it without restarting halmasuit.
    print("test-greeter: start_session with sleep", file=sys.stderr)
    send({
        "type": "start_session",
        "cmd": ["${pkgs.coreutils}/bin/sleep", "infinity"],
        "env": [],
    })
    r = recv()
    print(f"test-greeter: start_session response: {r}", file=sys.stderr)
    if r.get("type") != "success":
        print("test-greeter: start_session failed", file=sys.stderr)
        sys.exit(1)

    # Idle so halmasuit's greeter slot stays alive while the test
    # driver inspects state.
    while True:
        time.sleep(60)
  '';

  # Wrapper halmasuit fork+execs as the greeter user. Single-path
  # contract (no argv); the Python script does the work.
  testGreeter = pkgs.writeShellScript "halmasuit-login-flash-greeter" ''
    exec ${pkgs.python3}/bin/python3 ${testGreetdAuth}
  '';
in
pkgs.testers.runNixOSTest {
  name = "login-flash";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit;
        spawnPackage   = halmasuit-spawn;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        greeterCommand = "${testGreeter}";
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

      # test-user.nix sets uid=1000 with default group `users` (gid 100),
      # but halmasuit-spawn's UID_MIN floor refuses gid < 1000. Give the
      # test user a primary group with gid 1000 so spawn proceeds.
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

    # Greeter spawn lands in the journal — wait for that event before
    # capturing the "before" PID, so we know halmasuit is past its
    # init phase and into the steady-state main loop.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'",
        timeout=30,
    )

    halmasuit_pid_before = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert halmasuit_pid_before.isdigit() and int(halmasuit_pid_before) > 0, (
        f"halmasuit MainPID must be positive int, got {halmasuit_pid_before!r}"
    )
    print(f"GREETER PHASE: halmasuit pid={halmasuit_pid_before}")

    # Wait for the session. halmasuit-spawn execs the command with
    # argv[0] = absolute store path, so the kernel's `comm` field is
    # the truncated full path ("/nix/store/jjxn…") rather than the
    # basename. We can't usefully `pgrep -x sleep`; matching against
    # the full cmdline via `-f` works.
    machine.wait_until_succeeds(
        "pgrep -u test -f /bin/sleep",
        timeout=60,
    )

    session_pid = machine.succeed("pgrep -u test -f /bin/sleep").strip()
    halmasuit_pid_after = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    print(
        f"SESSION PHASE: halmasuit pid={halmasuit_pid_after} "
        f"session pid={session_pid}"
    )

    # The assertion: halmasuit's PID didn't change.
    if halmasuit_pid_before == halmasuit_pid_after:
        print(
            f"PASS: halmasuit pid {halmasuit_pid_before} continuous across "
            f"greeter→session boundary — no compositor restart, no flash"
        )
    else:
        raise AssertionError(
            "FLASH DETECTED — halmasuit restarted across login boundary:\n"
            f"  halmasuit pid: greeter={halmasuit_pid_before} → "
            f"session={halmasuit_pid_after}\n"
            "\n"
            "Under v2's architecture halmasuit is the long-lived\n"
            "compositor; its PID should be identical on both sides of\n"
            "the greeter→session transition. A PID change here means\n"
            "the unit was restarted (Restart=on-failure tripping, OOM,\n"
            "explicit stop) — which IS a flash in user-visible terms.\n"
        )
  '';
}
