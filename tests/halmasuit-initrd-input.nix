# tests/halmasuit-initrd-input.nix — input after switch-root (fromInitrd).
#
# The Phase B deployment opens every /dev/input/event* node ONCE, in
# the initramfs (`setup_libinput_direct`), and has no input hotplug.
# This gate drives a real QEMU keyboard into the post-pivot greeter and
# then reproduces the gnomon system event that loses it:
#
#   1. Stage 1 enumerated input (`libinput direct: enumerated …`,
#      device_count ≥ 1).
#   2. After switch-root AND `deprivileged`, the same halmasuit PID
#      still holds one input character-device fd (major 13) per
#      enumerated device. Counted by device number through
#      /proc/<pid>/fd, never by link text (the links are printed for
#      the record).
#   3. A `send_key` keystroke reaches the broker-spawned greeter
#      (`layer-shell-test-client: key press keysym=0x61`).
#   4. keyd (`ids = [ "*" ]`, gnomon's config) starts after halmasuit's
#      enumeration: it EVIOCGRABs the keyboard and re-emits through a
#      uinput "keyd virtual keyboard" halmasuit never opened. A second
#      keystroke must still reach the greeter.
#
# keyd is started by the test (not at boot) so steps 2-3 are observed
# before the grab; on gnomon it starts at boot, after halmasuit's
# stage-1 enumeration, which is the same ordering.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-initrd-input";

  skipTypeCheck = true;

  nodes.machine =
    { lib, pkgs, ... }:
    let
      # Test client as the greeter, in keyboard mode (exclusive keyboard
      # interactivity → halmasuit focuses it). Pinned in the rootfs store
      # below so the post-chroot broker spawn finds it.
      testGreeter = pkgs.writeShellScript "halmasuit-initrd-input-greeter" ''
        export HALMASUIT_TESTCLIENT_KEYBOARD=1
        exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
      '';
    in
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
        luks.package      = halmasuit-luks;
        session.package   = halmasuit-session;
        greeterCommand    = "${testGreeter}";
      };

      system.extraDependencies = [ testGreeter ];

      # gnomon's keyd shape: every keyboard grabbed. Started by the test
      # script, after the pre-grab assertions.
      services.keyd = {
        enable = true;
        keyboards.default.ids = [ "*" ];
      };
      systemd.services.keyd.wantedBy = lib.mkForce [ ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          # Evdev keyboard + relative mouse present in stage 1 (the
          # module loads evdev + virtio_input in the initramfs);
          # machine.send_key drives the keyboard.
          "-device virtio-keyboard-pci"
          "-device virtio-mouse-pci"
        ];
      };
    };

  testScript = ''
    import datetime
    import json
    import sys

    sys.path.insert(0, "${./lib}")
    import visual

    def journal(unit_args=""):
        return machine.succeed(f"journalctl -b {unit_args} -o cat --no-pager")

    def greeter_got(keysym):
        # The greeter is broker-spawned; its stderr lands in the
        # halmasuit-session journal, so match the whole boot.
        return (
            machine.execute(
                "journalctl -b -o cat --no-pager | grep -qF "
                f"'layer-shell-test-client: key press keysym={keysym}'"
            )[0]
            == 0
        )

    def input_fds(pid):
        # One "<fd> <major-hex> <link>" line per open fd whose target is
        # a character device. stat -L follows the /proc magic link to
        # the open file itself, so a detached mount still resolves.
        out = machine.succeed(
            f"for f in /proc/{pid}/fd/*; do "
            '[ -c "$f" ] && printf "%s %s %s\\n" "''${f##*/}" '
            '"$(stat -L -c %t "$f")" "$(readlink "$f")"; '
            "done; true"
        )
        rows = [line.split(" ", 2) for line in out.splitlines() if line]
        return [r for r in rows if r[1] == "d"]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "journalctl -b -u halmasuit -o cat --no-pager "
        "| grep -F '\\\"phase\\\":\\\"deprivileged\\\"'",
        timeout=60,
    )

    events = visual.introspect_events(machine)
    started = [e["pid"] for e in events if e["event"] == "started"]
    assert len(started) == 1, f"expected one halmasuit start, got {started}"
    pid = started[0]
    phases = [e["phase"] for e in events if e["event"] == "phase_entered"]
    assert "initramfs_init" in phases and "rootfs_ready" in phases, phases
    machine.succeed(f"kill -0 {pid}")
    print(f"halmasuit PID {pid} crossed switch-root; phases {phases}")

    # ── 1. stage-1 enumeration ──────────────────────────────────────
    enumerated = [
        json.loads(line)["fields"]["device_count"]
        for line in journal("-u halmasuit").splitlines()
        if "libinput direct: enumerated" in line
    ]
    assert enumerated == [enumerated[0]] and enumerated[0] >= 1, (
        f"expected one stage-1 enumeration with devices, got {enumerated}"
    )
    device_count = enumerated[0]
    print(f"PASS: stage-1 libinput enumerated {device_count} devices")

    # ── 2. input fds still held after switch-root + deprivileged ────
    held = input_fds(pid)
    print(f"halmasuit input fds after deprivileged: {held}")
    print(
        "/proc/bus/input/devices:\n"
        + machine.succeed("grep -E '^(N|H):' /proc/bus/input/devices")
    )
    assert len(held) == device_count, (
        f"halmasuit PID {pid} holds {len(held)} input fds after "
        f"switch-root + deprivileged; stage 1 enumerated {device_count}"
    )
    print(f"PASS: PID {pid} still holds all {device_count} input fds")

    # ── 3. a keystroke reaches the greeter ──────────────────────────
    machine.wait_until_succeeds(
        "journalctl -b -o cat --no-pager | grep -qF "
        "'layer-shell-test-client: keyboard enter'",
        timeout=60,
    )

    def press_until_seen(key, keysym, context):
        def attempt(last_try):
            machine.send_key(key)
            if greeter_got(keysym):
                return True
            if last_try:
                raise AssertionError(
                    f"{context}: send_key({key!r}) never reached the "
                    f"greeter (no 'key press keysym={keysym}'); halmasuit "
                    f"input fds now {input_fds(pid)}"
                )
            return False

        retry(attempt, timeout=datetime.timedelta(seconds=30))

    press_until_seen("a", "0x61", "before keyd")
    print("PASS: post-pivot keystroke reached the greeter")

    # ── 4. keyd grabs the keyboard ──────────────────────────────────
    machine.succeed("systemctl start keyd.service")
    machine.wait_until_succeeds(
        "journalctl -b -u keyd -o cat --no-pager | grep -F 'DEVICE: match'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "grep -qF 'keyd virtual keyboard' /proc/bus/input/devices", timeout=30
    )
    print(
        "keyd started:\n"
        + journal("-u keyd")
        + f"halmasuit input fds after keyd: {input_fds(pid)}"
    )
    press_until_seen("b", "0x62", "after keyd grabbed the keyboard")
    print("PASS: keystroke reached the greeter through keyd")
  '';
}
