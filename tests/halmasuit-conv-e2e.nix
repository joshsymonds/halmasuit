# halmasuit-conv-e2e: end-to-end gate on the compositor's
# `broker_relay::awaiting_display_ack` swallow flag.
#
# session-conv-shapes pins the contract on the BROKER wire (a python
# client speaks the broker SOCK_SEQPACKET protocol directly), and the
# broker_relay unit tests pin the contract at the sans-IO level. Neither
# drives a real greetd protocol exchange THROUGH halmasuit's
# broker_relay against a real broker emitting PAM_TEXT_INFO. This test
# fills that gap (Epic #28 Pass B / finding B-I1):
#
#   - PAM stack composed: pam_echo (info=cue) + pam_unix (try_first_pass).
#   - The "greeter" is a python client speaking the halmasuit-greetd wire
#     protocol over halmasuit's GREETD_SOCK (the same socket DMS uses).
#   - For every `auth_message`, the client sends `post_auth_message_response`
#     mimicking DMS (which responds to info/error too — that's the
#     greetd contract).
#   - halmasuit's `BrokerRelay` must:
#       (a) forward Challenge(Info) to the greeter on the PAM_TEXT_INFO
#           round; arm `awaiting_display_ack`;
#       (b) swallow the greeter's `respond("")` for the info round
#           (NOT forward a ConvResponse to the broker, which would land
#           on AwaitWorker → UnexpectedFrame → fail-closed);
#       (c) clear the flag on the next Challenge(Secret);
#       (d) forward the real ConvResponse for the secret round;
#       (e) reach AuthSuccess → StartSession → Spawning.
#
# If the swallow flag drifts at any step, the auth fails closed and this
# test reds.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  cueText = "Touch your device to continue";
  cueFile = pkgs.writeText "halmasuit-conv-e2e-cue" cueText;

  pamModule = m: "${pkgs.pam}/lib/security/${m}.so";

  # The greetd client: speaks halmasuit-greetd's wire protocol over
  # GREETD_SOCK. Asserts BOTH `info` AND `secret` auth_message types
  # traversed the compositor's broker_relay before AuthSuccess — proves
  # the swallow flag re-armed correctly.
  testGreetdClient = pkgs.writeScript "halmasuit-conv-e2e-client.py" ''
    #!${pkgs.python3}/bin/python3
    import json, os, socket, struct, sys, time

    sock_path = os.environ.get("GREETD_SOCK")
    if not sock_path:
        print("e2e-client: GREETD_SOCK not set", file=sys.stderr)
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

    print("e2e-client: create_session for user=test", file=sys.stderr)
    send({"type": "create_session", "username": "test"})

    info_seen = False
    secret_seen = False
    while True:
        r = recv()
        print(f"e2e-client: recv {r}", file=sys.stderr)
        kind = r.get("type")
        if kind == "auth_message":
            mtype = r.get("auth_message_type")
            mmsg = r.get("auth_message", "")
            # Mimic DMS: respond to every auth_message regardless of
            # type. The compositor's broker_relay decides whether to
            # forward as ConvResponse (visible/secret) or swallow
            # (info/error).
            if mtype == "info":
                info_seen = True
                if "Touch your device" not in mmsg:
                    print(f"e2e-client: info message lost cue text: {mmsg!r}", file=sys.stderr)
                    sys.exit(1)
                send({"type": "post_auth_message_response"})
            elif mtype == "error":
                send({"type": "post_auth_message_response"})
            elif mtype == "secret":
                secret_seen = True
                send({"type": "post_auth_message_response", "response": "test"})
            elif mtype == "visible":
                send({"type": "post_auth_message_response", "response": "test"})
            else:
                print(f"e2e-client: unknown auth_message_type {mtype!r}", file=sys.stderr)
                sys.exit(1)
        elif kind == "success":
            if not (info_seen and secret_seen):
                print(
                    f"e2e-client: AUTH_SUCCESS but never saw both info AND secret "
                    f"(info_seen={info_seen}, secret_seen={secret_seen}) — "
                    f"PAM stack misconfigured, or compositor short-circuited.",
                    file=sys.stderr,
                )
                sys.exit(1)
            print(
                "e2e-client: AUTH_SUCCESS — both PAM_TEXT_INFO and "
                "PAM_PROMPT_ECHO_OFF traversed halmasuit's broker_relay; "
                "swallow flag re-armed correctly.",
                file=sys.stderr,
            )
            break
        elif kind == "error":
            print(f"e2e-client: AUTH_ERROR {r}", file=sys.stderr)
            sys.exit(1)
        else:
            print(f"e2e-client: unexpected frame {r}", file=sys.stderr)
            sys.exit(1)

    send({
        "type": "start_session",
        "cmd": ["${pkgs.coreutils}/bin/sleep", "infinity"],
        "env": [],
    })
    r = recv()
    if r.get("type") != "success":
        print(f"e2e-client: start_session failed: {r}", file=sys.stderr)
        sys.exit(1)
    print("e2e-client: SESSION_STARTED", file=sys.stderr)

    # Idle so halmasuit's greeter slot stays occupied while the test
    # driver inspects state.
    while True:
        time.sleep(60)
  '';

  testGreeter = pkgs.writeShellScript "halmasuit-conv-e2e-greeter" ''
    exec ${pkgs.python3}/bin/python3 ${testGreetdClient}
  '';
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-conv-e2e";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable           = true;
        package          = halmasuit;
        session.package  = halmasuit-session;
        greeterUid       = 999;
        greeterGroup     = "halmasuit-greeter";
        compositorUid    = 998;
        greeterCommand   = "${testGreeter}";
        installPamConfig = false;
      };

      # Custom PAM service: pam_echo emits PAM_TEXT_INFO BEFORE
      # pam_unix's PAM_PROMPT_ECHO_OFF. Without pam_echo the conv is
      # just a single secret prompt and the swallow flag is never set;
      # adding pam_echo makes this the gen-399 production shape end-
      # to-end through halmasuit.
      security.pam.services.halmasuit.text = ''
        auth required ${pamModule "pam_echo"} file=${cueFile}
        auth required ${pamModule "pam_unix"} try_first_pass
        account required ${pamModule "pam_unix"}
        session required ${pamModule "pam_unix"}
      '';

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

      # Match login-flash: PAM-resolved primary gid must be ≥ UID_MIN.
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

    # Wait for the e2e client to finish auth via journal markers.
    machine.wait_until_succeeds(
        "journalctl | grep -qF 'e2e-client: AUTH_SUCCESS'",
        timeout=60,
    )

    machine.wait_until_succeeds(
        "journalctl | grep -qF 'e2e-client: SESSION_STARTED'",
        timeout=60,
    )

    machine.wait_until_succeeds(
        "pgrep -u test -f /bin/sleep",
        timeout=60,
    )

    print(
        "halmasuit-conv-e2e: ALL ASSERTIONS PASSED — pam_echo "
        "PAM_TEXT_INFO + pam_unix PAM_PROMPT_ECHO_OFF traversed "
        "halmasuit's broker_relay end-to-end; the compositor's "
        "awaiting_display_ack swallow flag forwarded the info round, "
        "swallowed the greeter's response, re-armed on the secret "
        "round, forwarded the real ConvResponse, and signin reached "
        "AuthSuccess → StartSession → Spawning. The gen-399 production "
        "failure mode is gated end-to-end through the compositor."
    )
  '';
}
