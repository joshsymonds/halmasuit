# tests/halmasuit-greeter-focus.nix — keyboard focus stays on one
# greeter layer surface while several of them repaint.
#
# DMS's Quickshell greeter maps one keyboard-interactive Overlay layer
# surface per output, and each one commits a new buffer whenever it
# repaints. Keyboard focus must not follow those commits: if it did,
# typed keys would split between the surfaces' text fields. Headless
# QEMU exposes one connected output, so the greeter here maps two
# such surfaces on that output: two layer-shell test clients that
# repaint on every frame callback. The test types twenty letters and
# asserts that every one reaches the same client and that the other
# client never receives keyboard focus.
#
# Which surface wins between outputs (the primary output's) is covered
# by the layer_focus unit tests.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
  client = "${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client";
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-greeter-focus";

  skipTypeCheck = true;

  nodes.machine =
    { pkgs, ... }:
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
        greeterCommand = "${pkgs.writeShellScript "halmasuit-focus-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=overlay
          export HALMASUIT_TESTCLIENT_CONTINUOUS=1
          HALMASUIT_TESTCLIENT_TAG=A ${client} &
          HALMASUIT_TESTCLIENT_TAG=B ${client} &
          wait
        ''}";
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

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          # An evdev keyboard libinput enumerates; send_chars drives it.
          "-device virtio-keyboard-pci"
        ];
      };
    };

  testScript = ''
    import re

    TYPED = "abcdefghijklmnopqrst"

    def client_lines(tag, text):
        # The clients are the broker-spawned greeter, so their stderr
        # lands in the halmasuit-session journal; match the whole boot.
        out = machine.succeed(
            "journalctl -b -o cat | grep -F "
            f"'layer-shell-test-client[{tag}]: {text}' || true"
        )
        return [line for line in out.splitlines() if line]

    def presses(tag):
        return [
            int(m.group(1), 16)
            for line in client_lines(tag, "key press keysym=")
            for m in [re.search(r"keysym=0x([0-9a-f]+)", line)]
            if m
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    for tag in ("A", "B"):
        machine.wait_until_succeeds(
            "journalctl -b -o cat | grep -qF "
            f"'layer-shell-test-client[{tag}]: painted'",
            timeout=60,
        )
        machine.wait_until_succeeds(
            "journalctl -b -o cat | grep -qF "
            f"'layer-shell-test-client[{tag}]: keyboard capability acquired'",
            timeout=60,
        )
    machine.wait_until_succeeds(
        "journalctl -b -o cat | grep -E "
        "'layer-shell-test-client\\[(A|B)\\]: keyboard enter'",
        timeout=30,
    )

    machine.send_chars(TYPED)

    machine.wait_until_succeeds(
        "test $(journalctl -b -o cat | grep -cE "
        f"'layer-shell-test-client\\[(A|B)\\]: key press keysym=') -ge {len(TYPED)}",
        timeout=60,
    )

    a, b = presses("A"), presses("B")
    print(f"halmasuit-greeter-focus: A got {len(a)} presses, B got {len(b)}")
    focused, other = ("A", "B") if a else ("B", "A")
    got = presses(focused)
    assert not presses(other), (
        f"keys split between greeter surfaces: {focused} got {len(got)}, "
        f"{other} got {len(presses(other))}"
    )
    assert got == [ord(c) for c in TYPED], f"{focused} got keysyms {got}"
    assert not client_lines(other, "keyboard enter"), (
        f"{other} received keyboard focus while {focused} held it"
    )

    print("halmasuit-greeter-focus: ALL ASSERTIONS PASSED")
  '';
}
