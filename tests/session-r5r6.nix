# Epic #1 R5 + R6 + Amendment A2 — the socket-activated broker posture
# gate.
#
# Proves, against the REAL libpam stack with the REAL test user (NO
# mock, NO PAM bypass — CLAUDE.md hard rule), that the deployed
# `halmasuit-session` systemd unit has the Amendment-A2 shape:
#
#   R6  No standing root when idle: before any connection the .socket
#       is active but the .service is dead and no `halmasuit-session`
#       process exists.
#   R6  On-demand activation: one greeter connection activates the
#       service; the broker drives REAL pam_unix far enough to emit a
#       conversation prompt (auth genuinely in flight).
#   R6  Idle-exit + lossless re-activation: with nothing in flight the
#       broker `exit(0)`s, the unit deactivates (no standing root
#       again), and the NEXT connection re-activates it.
#   R5  Evict-old reachable FROM THE BROKER PROCESS: with auth A in
#       flight (worker blocked in real pam_authenticate), a second
#       connection B from the SO_PEERCRED greeter uid is observed by
#       the event loop and EVICTS A's worker — the property the
#       pre-Amendment-A2 serial accept loop structurally could not
#       satisfy — while B's own auth proceeds.
#
# State-based throughout (`wait_until_succeeds`/`fail`), never
# `time.sleep` (memory feedback-state-based-polling).

{
  system,
  nixpkgs,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # SOCK_SEQPACKET client speaking the broker's framed wire codec
  # ([u32 native-endian length][json]). Sends one BeginAuth, reads the
  # broker's first frame (the relayed real-pam_unix conv prompt), then
  # either exits ("once") or blocks on a second recv ("hold") so the
  # auth stays in flight until the worker is evicted/killed.
  client = pkgs.writeText "broker-client.py" ''
    import socket, struct, sys, json
    PATH = "/run/halmasuit-session.sock"
    def frame(obj):
        b = json.dumps(obj, separators=(",", ":")).encode()
        return struct.pack("=I", len(b)) + b
    mode = sys.argv[1]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    s.connect(PATH)
    s.send(frame({"type": "begin_auth",
                  "service": "halmasuit-pam-test",
                  "username": "test"}))
    data = s.recv(65536)
    if not data:
        print("NOFRAME", flush=True); sys.exit(0)
    ln = struct.unpack("=I", data[:4])[0]
    msg = json.loads(data[4:4 + ln])
    print("FRAME " + msg.get("type", "?"), flush=True)
    if mode == "hold":
        more = s.recv(65536)
        print("EOF" if not more else "MORE", flush=True)
  '';
in
pkgs.testers.runNixOSTest {
  name = "session-r5r6";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      # Epic R8: resolved gid must be ≥ the broker UID floor. NixOS's
      # default `users` group is GID 100; give `test` a user-private
      # GID-1000 group (same convention as run-pam-auth.nix).
      users.groups.test = { gid = 1000; };
      users.users.test = { group = "test"; };

      # REAL pam_unix — not a mock. Module by absolute store path so
      # resolution never depends on PAM's search path.
      security.pam.services.halmasuit-pam-test.text = ''
        auth     required ${pkgs.pam}/lib/security/pam_unix.so
        account  required ${pkgs.pam}/lib/security/pam_unix.so
      '';

      services.halmasuit.session = {
        enable  = true;
        package = halmasuit-session;
      };
      # The broker's SO_PEERCRED gate authorizes exactly this uid; the
      # test user is uid 1000 (tests/lib/test-user.nix).
      services.halmasuit.greeterUid = 1000;
      services.halmasuit.pamService = "halmasuit-pam-test";

      environment.systemPackages = [ pkgs.python3 ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    machine.wait_for_unit("sockets.target")
    machine.wait_until_succeeds(
        "systemctl is-active halmasuit-session.socket"
    )

    # ── R6: no standing root when idle ──
    machine.fail("pgrep -x halmasuit-session")
    machine.succeed(
        '[ "$(systemctl is-active halmasuit-session.service)" != active ]'
    )

    # ── R6: on-demand activation; REAL pam_unix prompts ──
    # Run as the greeter uid (1000) so the SO_PEERCRED gate admits it.
    machine.succeed(
        "runuser -u test -- python3 ${client} hold > /tmp/a.out 2>&1 &"
    )
    machine.wait_until_succeeds("systemctl is-active halmasuit-session.service")
    machine.wait_until_succeeds("grep -q 'FRAME conv_prompt' /tmp/a.out")

    # ── R5: a second greeter connection EVICTS the in-flight worker ──
    # (the event-loop property the serial accept loop could not reach).
    machine.succeed(
        "runuser -u test -- python3 ${client} hold > /tmp/b.out 2>&1 &"
    )
    # B's own real auth reaches its conv prompt …
    machine.wait_until_succeeds("grep -q 'FRAME conv_prompt' /tmp/b.out")
    # … and A's worker was SIGKILLed by AuthSlot::create on B, so A's
    # held connection is torn down (broker drop_active → socket EOF).
    machine.wait_until_succeeds("grep -q 'EOF' /tmp/a.out")

    # Tear B down; nothing left in flight.
    machine.succeed("pkill -f '[b]roker-client.py' || true")

    # ── R6: idle-exit → unit deactivates (no standing root) ──
    # The broker idle window is 30s; poll generously (state-based, not
    # a sleep — we assert the END state, however long it takes).
    machine.wait_until_succeeds(
        '[ "$(systemctl is-active halmasuit-session.service)" != active ]',
        timeout=120,
    )
    machine.fail("pgrep -x halmasuit-session")

    # ── R6: lossless re-activation by the retained socket ──
    machine.succeed(
        "runuser -u test -- python3 ${client} once > /tmp/c.out 2>&1 &"
    )
    machine.wait_until_succeeds("systemctl is-active halmasuit-session.service")
    machine.wait_until_succeeds("grep -q 'FRAME conv_prompt' /tmp/c.out")
    machine.succeed("pkill -f '[b]roker-client.py' || true")

    print(
        "session-r5r6: socket-activated halmasuit-session has the "
        "Amendment-A2 posture — no standing root when idle, on-demand "
        "activation driving REAL pam_unix, idle-exit + lossless "
        "re-activation, and evict-old reachable from the broker event "
        "loop (a second greeter connection SIGKILLs the in-flight "
        "worker)."
    )
  '';
}
