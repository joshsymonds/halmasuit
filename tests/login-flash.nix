# login-flash: the canonical v1 red test.
#
# Boots the user's dms-niri stack with a CUSTOM TEST GREETER (not
# DankGreeter, because that needs a rendering UI we don't have on
# virtio-gpu-pci in headless tests). The custom greeter wraps niri the
# same way DankGreeter does — wrapper → niri → spawned auth client —
# but auto-drives greetd's wire protocol instead of waiting for human
# input.
#
# The flash being measured is greetd's own act of killing the greeter
# process tree and exec'ing the session. That behaviour is identical
# regardless of which greeter is in the slot. So substituting the
# greeter does NOT compromise the measurement; it just makes the test
# deterministic in CI.
#
# Captures niri's PID + UID on greeter and session sides; asserts
# continuity. Fails today because greetd restarts the compositor.
# That failure is the v1 baseline.

{
  system,
  nixpkgs,
  nix-config,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };

  testInputs = nix-config.inputs // {
    inherit nix-config;
  };

  # The Python script that drives greetd's wire protocol from inside
  # the greeter process. greetd's protocol is 4-byte LE length-prefixed
  # JSON over a Unix socket. https://man.sr.ht/~kennylevinsen/greetd/protocol.md
  testGreetdAuth = pkgs.writeScript "test-greetd-auth.py" ''
    #!${pkgs.python3}/bin/python3
    import json, os, socket, struct, sys, time

    sock_path = os.environ.get("GREETD_SOCK")
    if not sock_path:
        print("GREETD_SOCK not set in greeter env", file=sys.stderr)
        sys.exit(1)

    # Give the test driver a window to snapshot greeter-side process
    # state (niri PID, /sys/kernel/debug/dri/0/clients) before we drive
    # the transition.
    time.sleep(6)

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

    print("test-greeter: sending create_session for user=test", file=sys.stderr)
    send({"type": "create_session", "username": "test"})

    while True:
        r = recv()
        print(f"test-greeter: recv {r}", file=sys.stderr)
        kind = r.get("type")
        if kind == "auth_message":
            # All auth messages get answered with "test". Visible-text
            # prompts (password) take the response; info/error prompts
            # accept any non-null and move on.
            send({"type": "post_auth_message_response", "response": "test"})
        elif kind == "success":
            # First "success" is auth-complete; second is start-session.
            # Decide by which we've already sent.
            break
        elif kind == "error":
            print(f"test-greeter: auth error: {r}", file=sys.stderr)
            sys.exit(1)
        else:
            print(f"test-greeter: unexpected {r}", file=sys.stderr)
            sys.exit(1)

    print("test-greeter: auth OK, sending start_session for niri", file=sys.stderr)
    send({"type": "start_session", "cmd": ["niri"], "env": []})
    r = recv()
    print(f"test-greeter: start_session response: {r}", file=sys.stderr)
    if r.get("type") != "success":
        print("test-greeter: start_session failed", file=sys.stderr)
        sys.exit(1)
    sys.exit(0)
  '';

  # niri config for the greeter side. Spawns our auth driver as a
  # child, then quits when it returns. (greetd will tear us down at
  # start_session before we reach the quit, which is the point.)
  testGreeterNiriConfig = pkgs.writeText "test-greeter-niri.kdl" ''
    hotkey-overlay {
        skip-at-startup
    }
    debug {
        keep-max-bpc-unchanged
    }
    layout {
        background-color "#000000"
    }
    spawn-at-startup "sh" "-c" "${testGreetdAuth}; niri msg action quit --skip-confirmation"
  '';

  # Wrapper script that greetd execs as the greeter. Sets up PATH so
  # niri's spawn-at-startup can find niri itself (for msg action quit),
  # then execs niri with our config. Mirrors dms-greeter's shape.
  niriPkg = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;
  testGreeter = pkgs.writeShellScript "test-greeter" ''
    set -e
    export PATH=${pkgs.lib.makeBinPath [ niriPkg pkgs.python3 pkgs.coreutils ]}:$PATH
    exec ${niriPkg}/bin/niri -c ${testGreeterNiriConfig}
  '';
in
pkgs.testers.runNixOSTest {
  name = "login-flash";

  node.specialArgs = { inputs = testInputs; };

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        "${nix-config}/modules/desktop/dms-niri.nix"
      ];

      desktop.dms-niri = {
        enable = true;
        # Disable DankGreeter — we substitute our deterministic test
        # greeter so auth doesn't depend on Quickshell rendering.
        greeter.enable = false;
      };

      # Configure greetd manually with the test greeter.
      services.greetd = {
        enable = true;
        settings.default_session = {
          command = "${testGreeter}";
          user = "greeter";
        };
      };

      users.users.test = {
        isNormalUser = true;
        password = "test";
        uid = 1000;
        extraGroups = [ "wheel" "video" "input" ];
      };

      security.sudo.wheelNeedsPassword = false;

      virtualisation = {
        memorySize = 2048;
        cores = 2;
        diskSize = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    # ── Phase 1: boot to greeter ────────────────────────────────────────
    machine.start()
    machine.wait_for_unit("graphical.target")
    machine.succeed("systemctl is-active greetd")

    # Wait for the greeter-side niri (running as user "greeter"). Our
    # test greeter sleeps 6s before driving auth, so we have a comfortable
    # window to snapshot state.
    machine.wait_until_succeeds("pgrep -u greeter -x niri", timeout = 30)
    greeter_pid = machine.succeed("pgrep -u greeter -x niri").strip()
    greeter_uid = machine.succeed(f"stat -c %u /proc/{greeter_pid}").strip()

    drm_pre = machine.succeed(
        "cat /sys/kernel/debug/dri/0/clients 2>/dev/null "
        "|| echo '(debugfs unreadable as test user — needs root inside VM)'"
    ).strip()

    print(f"GREETER SIDE: niri pid={greeter_pid} uid={greeter_uid}")
    print(f"DRM clients pre-auth:\n{drm_pre}")

    # ── Phase 2: wait for the user session ──────────────────────────────
    # The test-greeter's Python script (running inside the greeter's niri)
    # connects to greetd's socket after 6s and drives auth automatically.
    # When start_session lands, greetd kills the greeter tree and execs
    # niri for user "test" (uid 1000). 90s timeout covers a cold cache.
    machine.wait_until_succeeds("pgrep -u test -x niri", timeout = 90)

    session_pid = machine.succeed("pgrep -u test -x niri").strip()
    session_uid = machine.succeed(f"stat -c %u /proc/{session_pid}").strip()

    drm_post = machine.succeed(
        "cat /sys/kernel/debug/dri/0/clients 2>/dev/null "
        "|| echo '(debugfs unreadable)'"
    ).strip()

    print(f"SESSION SIDE: niri pid={session_pid} uid={session_uid}")
    print(f"DRM clients post-auth:\n{drm_post}")

    # ── Phase 3: the assertion ──────────────────────────────────────────
    if greeter_pid == session_pid and greeter_uid == session_uid:
        print("PASS: niri PID + UID continuous across login boundary "
              "(no flash detected)")
    else:
        raise AssertionError(
            "FLASH DETECTED — compositor restarted across login boundary:\n"
            f"  niri PID: greeter={greeter_pid} → session={session_pid}\n"
            f"  niri UID: greeter={greeter_uid} → session={session_uid}\n"
            "\n"
            "On the current greetd flow this failure is expected — greetd\n"
            "kills the greeter's compositor and execs a fresh compositor\n"
            "for the user session. v2 of halmasuit (a long-lived system\n"
            "compositor that hosts both phases) eliminates the restart;\n"
            "this test then goes green.\n"
        )
  '';
}
