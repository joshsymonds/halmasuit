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
#      still holds one stage-1 input character-device fd (major 13) per
#      enumerated device, and no input device twice (the post-pivot
#      rescan adds devices that appeared later, never a held one).
#      Devices are matched by number through /proc/<pid>/fd; link text
#      only tells a stage-1 fd (detached "/input/eventN") from a
#      post-pivot one.
#   2b. On every thread of that PID: no supplementary group, gids
#      compositor/compositor/input/compositor (the `input` gid only as
#      the saved set-gid) and `CapBnd` 0.
#   3. A `send_key` keystroke reaches the broker-spawned greeter
#      (`layer-shell-test-client: key press keysym=0x61`).
#   3b. Before keyd starts, a uinput keyboard created after the greeter
#      has keyboard focus is opened by halmasuit (an fd with its device
#      number), a key typed on it reaches the greeter, and once the
#      device is destroyed halmasuit no longer holds its device number.
#   4. keyd (`ids = [ "*" ]`, gnomon's config) starts after halmasuit's
#      enumeration: it EVIOCGRABs the keyboard and re-emits through a
#      uinput "keyd virtual keyboard" halmasuit never opened. A second
#      keystroke must still reach the greeter.
#   5. `systemctl restart keyd`: a keystroke still reaches the greeter,
#      and halmasuit's held input fds have no duplicate device number
#      and each matches a live /dev/input/event* node.
#   6. The step-2b credentials still hold after the post-drop opens
#      (each open's fsgid bracket restored the compositor gid).
#
# keyd is started by the test (not at boot) so steps 2-3b are observed
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

  pythonWithEvdev = pkgs.python3.withPackages (ps: [ ps.evdev ]);

  # A uinput keyboard that lives as long as its process. It writes its
  # event node path to argv[1] once created; SIGUSR1 types "c"; SIGTERM
  # (the unit stop) exits and the kernel destroys the device.
  lateKeyboard = pkgs.writeScript "halmasuit-initrd-input-late-keyboard" ''
    #!${pythonWithEvdev}/bin/python3
    import signal
    import sys

    from evdev import UInput, ecodes

    ui = UInput({ecodes.EV_KEY: [ecodes.KEY_C]}, name="halmasuit late keyboard")


    def press_c(*_):
        ui.write(ecodes.EV_KEY, ecodes.KEY_C, 1)
        ui.write(ecodes.EV_KEY, ecodes.KEY_C, 0)
        ui.syn()


    signal.signal(signal.SIGUSR1, press_c)
    with open(sys.argv[1], "w") as f:
        f.write(ui.device.path)
    while True:
        signal.pause()
  '';
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

      system.extraDependencies = [ testGreeter lateKeyboard ];

      # gnomon's keyd shape: every keyboard grabbed. Started by the test
      # script, after the pre-grab assertions.
      services.keyd = {
        enable = true;
        keyboards.default.ids = [ "*" ];
      };
      systemd.services.keyd.wantedBy = lib.mkForce [ ];

      # The late uinput keyboard in step 3b is created through /dev/uinput.
      boot.kernelModules = [ "uinput" ];

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
        # One "<fd> <major-hex> <minor-hex> <link>" line per open fd whose
        # target is a character device. stat -L follows the /proc magic
        # link to the open file itself, so a detached mount still resolves.
        out = machine.succeed(
            f"for f in /proc/{pid}/fd/*; do "
            '[ -c "$f" ] && printf "%s %s %s %s\\n" "''${f##*/}" '
            '"$(stat -L -c %t "$f")" "$(stat -L -c %T "$f")" "$(readlink "$f")"; '
            "done; true"
        )
        rows = [line.split(" ", 3) for line in out.splitlines() if line]
        return [r for r in rows if r[1] == "d"]

    def devnum(path):
        major, minor = machine.succeed(f"stat -L -c '%t %T' {path}").split()
        return (major, minor)

    def live_devnums():
        # A node may vanish between the glob and its stat; stat still
        # prints every node it could read.
        _, out = machine.execute("stat -L -c '%t %T' /dev/input/event* 2>/dev/null")
        return {tuple(line.split()) for line in out.splitlines() if line}

    def status_field(status, name):
        for line in status.splitlines():
            if line.startswith(name + ":"):
                return line.split(":", 1)[1].split()
        raise AssertionError(f"no {name}: line in /proc status")

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
    # Fds opened in stage 1 resolve to the detached stage-1 /dev mount
    # ("/input/eventN"); the post-pivot rescan opens devices that
    # appeared after stage 1 (here the USB tablet and VMMouse) under
    # /dev/input. The link text only classifies an fd's origin; devices
    # are matched by number.
    stage1 = [r for r in held if not r[3].startswith("/dev/")]
    assert len(stage1) == device_count, (
        f"halmasuit PID {pid} holds {len(stage1)} stage-1 input fds after "
        f"switch-root + deprivileged; stage 1 enumerated {device_count}"
    )
    devs = [(r[1], r[2]) for r in held]
    assert len(devs) == len(set(devs)), (
        f"halmasuit PID {pid} holds an input device twice after the "
        f"post-pivot rescan: {held}"
    )
    print(
        f"PASS: PID {pid} still holds all {device_count} stage-1 input fds, "
        f"no device twice"
    )

    # ── 2b. compositor gids, groups and bounding set ────────────────
    input_gid = machine.succeed("getent group input | cut -d: -f3").strip()

    def assert_creds(context):
        # Every thread: no supplementary group, gids compositor/
        # compositor/input/compositor (real/effective/saved/fs: the
        # `input` gid is reachable only as the saved set-gid, so exec'd
        # children never inherit it), and an empty bounding set.
        tids = machine.succeed(f"ls /proc/{pid}/task").split()
        violations = []
        audited = 0
        for tid in tids:
            # A short-lived thread may exit between the listing and the read.
            rc, status = machine.execute(f"cat /proc/{pid}/task/{tid}/status")
            if rc != 0:
                continue
            audited += 1
            gids = status_field(status, "Gid")
            groups = status_field(status, "Groups")
            capbnd = int(status_field(status, "CapBnd")[0], 16)
            compositor = gids[0]
            if (
                gids != [compositor, compositor, input_gid, compositor]
                or compositor in (input_gid, "0")
                or groups
                or capbnd != 0
            ):
                violations.append(
                    f"thread {tid}: Gid: {' '.join(gids)} "
                    f"Groups: {' '.join(groups)} CapBnd: {capbnd:016x}"
                )
        assert audited and not violations, (
            f"{context}: halmasuit PID {pid} must hold, on every thread, "
            f"Gid compositor/compositor/{input_gid}/compositor (input gid "
            f"saved only), no supplementary groups and CapBnd 0; audited "
            f"{audited}; observed " + "; ".join(violations)
        )
        print(
            f"PASS ({context}): PID {pid} gids c/c/{input_gid}/c, no groups, "
            f"CapBnd 0 on {audited} threads"
        )

    assert_creds("after deprivileged")

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

    # ── 3b. a late uinput keyboard, created after greeter focus ─────
    machine.succeed(
        "systemd-run --unit=halmasuit-late-kbd --collect "
        "${lateKeyboard} /run/halmasuit-late-kbd.node"
    )
    machine.wait_until_succeeds("test -s /run/halmasuit-late-kbd.node", timeout=30)
    late_node = machine.succeed("cat /run/halmasuit-late-kbd.node").strip()
    late_dev = devnum(late_node)

    def late_held():
        return any((r[1], r[2]) == late_dev for r in input_fds(pid))

    def await_late_open(last_try):
        if late_held():
            return True
        if last_try:
            raise AssertionError(
                f"halmasuit PID {pid} never opened the late uinput keyboard "
                f"{late_node} (device {late_dev}); input fds now {input_fds(pid)}"
            )
        return False

    retry(await_late_open, timeout=datetime.timedelta(seconds=30))
    print(f"PASS: PID {pid} opened the late uinput keyboard {late_node} {late_dev}")

    def press_late_key(last_try):
        machine.succeed("systemctl kill -s SIGUSR1 halmasuit-late-kbd.service")
        if greeter_got("0x63"):
            return True
        if last_try:
            raise AssertionError(
                f"late uinput keyboard {late_node}: key 'c' never reached the "
                f"greeter (no 'key press keysym=0x63'); halmasuit input fds "
                f"now {input_fds(pid)}"
            )
        return False

    retry(press_late_key, timeout=datetime.timedelta(seconds=30))
    print("PASS: keystroke on the late uinput keyboard reached the greeter")

    machine.succeed("systemctl stop halmasuit-late-kbd.service")

    def await_late_released(last_try):
        if not late_held():
            return True
        if last_try:
            raise AssertionError(
                f"late uinput keyboard {late_node} was destroyed but halmasuit "
                f"PID {pid} still holds device {late_dev}; input fds now "
                f"{input_fds(pid)}"
            )
        return False

    retry(await_late_released, timeout=datetime.timedelta(seconds=30))
    print(f"PASS: PID {pid} released device {late_dev} after the late keyboard was destroyed")

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

    # ── 5. keyd restart: keystroke reaches the greeter, fds coherent ─
    machine.succeed("systemctl restart keyd.service")
    # Press only once the restarted keyd holds its grab: before it, a
    # keystroke would still arrive on halmasuit's stage-1 fd for the
    # physical keyboard. After it, only an fd on the new virtual
    # keyboard can deliver the key (the old one's device number may be
    # reused, so the held-fd wait is readiness, the press is the proof).
    machine.wait_until_succeeds(
        "journalctl -b -o cat --no-pager "
        "_SYSTEMD_INVOCATION_ID=$(systemctl show -p InvocationID --value keyd) "
        "| grep -F 'DEVICE: match'",
        timeout=30,
    )

    def keyd_virtual_devnum():
        rc, handlers = machine.execute(
            "grep -A4 -F 'N: Name=\"keyd virtual keyboard\"' "
            "/proc/bus/input/devices | grep '^H:'"
        )
        nodes = [h for h in handlers.split() if h.startswith("event")]
        if rc != 0 or len(nodes) != 1:
            return None
        return devnum(f"/dev/input/{nodes[0]}")

    def await_keyd_virtual_held(last_try):
        dev = keyd_virtual_devnum()
        if dev is not None and any((r[1], r[2]) == dev for r in input_fds(pid)):
            return True
        if last_try:
            raise AssertionError(
                f"halmasuit PID {pid} never opened the restarted keyd virtual "
                f"keyboard (device {dev}); input fds now {input_fds(pid)}"
            )
        return False

    retry(await_keyd_virtual_held, timeout=datetime.timedelta(seconds=30))
    press_until_seen("d", "0x64", "after keyd restart")
    print("PASS: keystroke reached the greeter after keyd restart")

    def fds_coherent(last_try):
        rows = input_fds(pid)
        devs = [(r[1], r[2]) for r in rows]
        live = live_devnums()
        duplicates = len(devs) != len(set(devs))
        stale = [d for d in devs if d not in live]
        if not duplicates and not stale:
            return True
        if last_try:
            raise AssertionError(
                f"halmasuit PID {pid} input fds after keyd restart: "
                f"duplicate device numbers: {duplicates}; device numbers with "
                f"no live /dev/input/event* node: {stale}; fds {rows}; live "
                f"{sorted(live)}"
            )
        return False

    retry(fds_coherent, timeout=datetime.timedelta(seconds=30))
    print(f"PASS: PID {pid} input fds are unique and all live after keyd restart")

    # ── 6. credentials unchanged after post-drop hotplug opens ──────
    # Steps 3b and 5 opened nodes after the drop with the fsgid set to
    # the input gid; every thread's fsgid must be back to compositor.
    assert_creds("after post-drop hotplug opens")
  '';
}
