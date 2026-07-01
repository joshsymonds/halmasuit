# visual-wallpaper-event: reactive-wallpaper bus gate.
#
# Proves the lifecycle bus drives the wallpaper shader's uniforms end to
# end, headless. A shader wallpaper declares an `event_time` uniform
# (uLoginTime) bound to `halmasuit.session.opened`. We drive a real
# greeter→session login (same auth driver as login-flash); when the
# session opens, halmasuit's wallpaper-event consumer writes the login
# timestamp into uLoginTime and emits a `wallpaper_uniform_applied`
# marker. Pixels are unobservable under virtio-gpu-pci, so the gate keys
# off that journald marker (the marker exists for exactly this reason).
#
# This complements the drm::centered_origin / shader::apply_event unit
# tests with a real-boot, real-ShaderBackend, real-emit() assertion.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # Auth driver: drives halmasuit's greetd wire to a session for user
  # `test`, then idles (identical contract to login-flash).
  testGreetdAuth = pkgs.writeScript "wallpaper-event-greetd-auth.py" ''
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

    send({"type": "create_session", "username": "test"})
    while True:
        r = recv()
        kind = r.get("type")
        if kind == "auth_message":
            send({"type": "post_auth_message_response", "response": "test"})
        elif kind == "success":
            break
        elif kind == "error":
            print(f"auth error: {r}", file=sys.stderr)
            sys.exit(1)
        else:
            print(f"unexpected {r}", file=sys.stderr)
            sys.exit(1)

    send({
        "type": "start_session",
        "cmd": ["${pkgs.coreutils}/bin/sleep", "infinity"],
        "env": [],
    })
    r = recv()
    if r.get("type") != "success":
        print("start_session failed", file=sys.stderr)
        sys.exit(1)

    while True:
        time.sleep(60)
  '';

  testGreeter = pkgs.writeShellScript "wallpaper-event-greeter" ''
    exec ${pkgs.python3}/bin/python3 ${testGreetdAuth}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-wallpaper-event";

  # The testScript imports tests/lib/visual.py via a runtime
  # sys.path.insert; the driver's static type checker can't resolve that
  # (same as visual-foreground / the phase-b cells).
  skipTypeCheck = true;

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
        # A shader wallpaper with a bus-driven EventTime uniform. The
        # consumer writes uLoginTime when halmasuit.session.opened fires.
        wallpaper = {
          type   = "shader";
          source = ./fixtures/wallpaper-event-shader.glsl;
          uniforms = {
            uLoginTime = {
              kind  = "event_time";
              event = "halmasuit.session.opened";
            };
          };
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

      # Broker UID_MIN floor refuses gid < 1000 (Epic R8/R11).
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
    import sys

    sys.path.insert(0, "${./lib}")
    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Drive to a session (the auth driver runs as halmasuit's greeter).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'",
        timeout=30,
    )
    machine.wait_until_succeeds("pgrep -u test -f /bin/sleep", timeout=60)

    # session_opened fires SessionOpened → the wallpaper-event consumer
    # resolves it to halmasuit.session.opened, writes uLoginTime, and
    # emits wallpaper_uniform_applied. Wait for that marker.
    machine.wait_until_succeeds(
        "journalctl -b -u halmasuit --output=cat --no-pager "
        "| grep -qF '\\\"event\\\":\\\"wallpaper_uniform_applied\\\"'",
        timeout=60,
    )

    # Assert the marker carries the expected event_name + uniform.
    applied = [
        e for e in visual.introspect_events(machine)
        if e["event"] == "wallpaper_uniform_applied"
        and e.get("event_name") == "halmasuit.session.opened"
        and e.get("uniform") == "uLoginTime"
    ]
    assert applied, (
        "no wallpaper_uniform_applied{event_name=halmasuit.session.opened, "
        "uniform=uLoginTime}; uniform-applied events seen: "
        + repr([
            (e.get("event_name"), e.get("uniform"))
            for e in visual.introspect_events(machine)
            if e["event"] == "wallpaper_uniform_applied"
        ])
    )
    print(
        "PASS: reactive wallpaper wrote uLoginTime on halmasuit.session.opened "
        f"({len(applied)} marker(s))"
    )
  '';
}
