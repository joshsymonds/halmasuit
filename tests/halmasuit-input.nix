# tests/halmasuit-input.nix — epic layer E2 gate.
#
# Proves a REAL emulated keystroke travels: QEMU evdev → libinput
# (device fd brokered by the same seatd/libseat session E1
# established) → halmasuit's wl_seat keyboard → the keyboard-focused
# wl_client. No mock. The test client requests exclusive keyboard
# interactivity (HALMASUIT_TESTCLIENT_KEYBOARD=1); halmasuit's
# focus policy routes keys to it; it logs every keysym it receives.
#
# Production `halmasuit` (input/seat is core, NOT frame_audit-gated).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-input";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit;
        session.package   = halmasuit-session;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        # Test client as the greeter, in keyboard mode (exclusive
        # keyboard interactivity → halmasuit focuses it).
        greeterCommand = "${pkgs.writeShellScript "halmasuit-input-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
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
          # An evdev keyboard libinput enumerates; machine.send_key
          # drives it.
          "-device virtio-keyboard-pci"
        ];
      };
    };

  testScript = ''
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    # Client bound the wl_seat keyboard …
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'layer-shell-test-client: keyboard capability acquired'",
        timeout=30,
    )
    # … and halmasuit's focus policy gave it keyboard focus (enter).
    # Waiting for enter avoids racing the keystroke ahead of focus.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'layer-shell-test-client: keyboard enter'",
        timeout=30,
    )

    # Inject a REAL 'a' keypress (QEMU evdev). xkb keysym for 'a'
    # is 0x61. Repeat a few times to beat any settle race.
    for _ in range(5):
        machine.send_key("a")
        time.sleep(0.4)

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'layer-shell-test-client: key press keysym=0x61'",
        timeout=30,
    )

    print("halmasuit-input: ALL ASSERTIONS PASSED")
  '';
}
