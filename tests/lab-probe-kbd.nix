# lab-probe-kbd gate (J1 + J2): with halmasuit.lab.probe.earlyKeyboard, the
# real lab-probe-kbd holder runs from stage 1 and lab-probe-type, installed on
# the rootfs PATH, relays requests to it. A test-owned evdev sink grabs the
# holder's keyboard and writes each typed line to a root-only file. The
# password is generated in the guest and reaches only the files and command
# pipelines that need it. It is scanned for in the journal and in every
# /proc/<pid>/cmdline, and both scans are proven live with a canary.
{
  system,
  nixpkgs,
  labModule,
}:

let
  pkgs = import nixpkgs {
    inherit system;
  };

  # Holds the keyboard's typed lines. It grabs the device so no tty sees the
  # keys, maps key codes back to a-z with its own literal table, applies
  # BackSpace to the field, and writes each Enter-terminated line to the sink
  # file. It prints nothing to stdout or stderr that carries typed text.
  sink = pkgs.writeText "lab-probe-sink.py" ''
    import fcntl
    import glob
    import os
    import struct
    import sys

    # The Linux input event codes for a-z, Enter and BackSpace.
    EVIOCGRAB = 0x40044590
    DEVICE_NAME = "lab-probe keyboard"
    EV_KEY = 1
    KEY_BACKSPACE = 14
    KEY_ENTER = 28
    LETTERS = {
        30: "a", 48: "b", 46: "c", 32: "d", 18: "e", 33: "f", 34: "g", 35: "h",
        23: "i", 36: "j", 37: "k", 38: "l", 50: "m", 49: "n", 24: "o", 25: "p",
        16: "q", 19: "r", 31: "s", 20: "t", 22: "u", 47: "v", 17: "w", 45: "x",
        21: "y", 44: "z",
    }
    # struct input_event on a 64-bit host: timeval, type, code, value.
    EVENT = struct.Struct("@llHHi")

    def find_keyboard():
        for path in sorted(glob.glob("/sys/class/input/event*/device/name")):
            with open(path) as f:
                if f.read().strip() == DEVICE_NAME:
                    return "/dev/input/" + path.split("/")[4]
        raise SystemExit("sink: keyboard not found")

    def main():
        out = os.open(sys.argv[1], os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        fd = os.open(find_keyboard(), os.O_RDONLY)
        fcntl.ioctl(fd, EVIOCGRAB, 1)
        with open(sys.argv[2], "w") as f:
            f.write("ready\n")
        field = []
        while True:
            data = os.read(fd, EVENT.size)
            if len(data) != EVENT.size:
                raise SystemExit("sink: short event")
            _sec, _usec, etype, code, value = EVENT.unpack(data)
            if etype != EV_KEY or value != 1:
                continue
            if code == KEY_ENTER:
                os.write(out, ("".join(field) + "\n").encode())
                field = []
            elif code == KEY_BACKSPACE:
                if field:
                    field.pop()
            elif code in LETTERS:
                field.append(LETTERS[code])

    main()
  '';

  # Scans every process's argv for the password and for the canary, through the
  # whole run. Each process is reported once, by pid, and the monitor never
  # writes the password. It runs as a transient unit started by the test.
  argvMonitor = pkgs.writeText "lab-probe-argv-monitor.py" ''
    import os
    import sys
    import time

    def read(path):
        with open(path, "rb") as f:
            return f.read().strip()

    secret = read(sys.argv[1])
    canary = read(sys.argv[2])
    if not secret or not canary:
        raise SystemExit("monitor: empty needle")
    log = os.open(sys.argv[3], os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    me = os.getpid()
    leaks = set()
    canaries = set()

    def note(line):
        os.write(log, line.encode() + b"\n")

    while True:
        for entry in os.listdir("/proc"):
            if not entry.isdigit() or int(entry) == me:
                continue
            try:
                with open("/proc/" + entry + "/cmdline", "rb") as f:
                    argv = f.read()
            except OSError:
                continue
            if secret in argv and entry not in leaks:
                leaks.add(entry)
                note("leak pid=" + entry)
            if canary in argv and entry not in canaries:
                canaries.add(entry)
                note("canary pid=" + entry)
        time.sleep(0.01)
  '';

  # Prints found when the needle file's bytes occur in stdin, absent when they
  # do not. The verdict is the output, so a driver command never fails on a match.
  needle = pkgs.writeText "lab-probe-needle.py" ''
    import sys

    with open(sys.argv[1], "rb") as f:
        needle = f.read().strip()
    if not needle:
        raise SystemExit("needle: empty")
    print("found" if needle in sys.stdin.buffer.read() else "absent")
  '';
in
pkgs.testers.runNixOSTest {
  name = "lab-probe-kbd";
  nodes.machine =
    { lib, ... }:
    {
      imports = [ labModule ];
      virtualisation = {
        memorySize = 1024;
        cores = 1;
      };
      # systemd-in-initramfs is required for boot.initrd.systemd.services.
      boot.initrd.systemd.enable = true;
      # The fixture has no LUKS root. The lab module's crypttab options name
      # cryptroot, so the device set is emptied as the EC2 class does.
      boot.initrd.luks.devices = lib.mkForce { };
      halmasuit.lab = {
        enable = true;
        # Units that exist in the VM and stay active, so the rootfs deadline
        # never reboots the machine during the test.
        displayManagerUnit = "multi-user.target";
        wolInterface = "eth1";
        probe = {
          enable = true;
          earlyKeyboard = true;
        };
      };
    };
  testScript = ''
    SINK = "/run/lab-probe-test/sink"
    PASSWORD = "/run/lab-probe-test/password"
    CANARY = "/run/lab-probe-test/canary"
    ARGV_LOG = "/run/lab-probe-test/argv.log"
    JOURNAL = "/run/lab-probe-test/journal.export"
    PYTHON = "${pkgs.python3}/bin/python3"

    # Shell fragments for the sink's expected stream. Each fragment prints the
    # bytes one request types into the sink, and the password is read by cat
    # from its file, so the password never appears in a command line.
    USER_PASS = [
        "printf 'labuser\\n'",
        "cat " + PASSWORD,
        "printf '\\n'",
    ]
    USER_FIXED = ["printf 'labuser\\nwrongprobe\\n'"]

    def sink_is(*parts):
        return "{ " + "; ".join(parts) + "; } | cmp -s - " + SINK

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("lab-probe-kbd.service")
    machine.wait_until_succeeds(
        "grep -qx 'lab-probe keyboard' /sys/class/input/event*/device/name",
        timeout=300,
    )

    # A 32-letter password from /dev/urandom, in a root-only file. The letters
    # are the a-z the holder accepts.
    machine.succeed(
        "install -d -m 0700 /run/lab-probe-test"
        " && (umask 077; head -c 2048 /dev/urandom | tr -dc 'a-z' | cut -c1-32 | tr -d '\\n' > "
        + PASSWORD + ")"
        " && test \"$(wc -c < " + PASSWORD + ")\" -eq 32"
    )
    machine.succeed("(umask 077; printf '%s' lab-canary-argv-7f3a > " + CANARY + ")")

    # The argv monitor runs before the first request and stays up to the end.
    machine.succeed(
        "systemd-run --unit=lab-probe-argv-monitor " + PYTHON + " " + "${argvMonitor}"
        + " " + PASSWORD + " " + CANARY + " " + ARGV_LOG
    )
    machine.succeed(
        "systemd-run --unit=lab-probe-canary " + PYTHON
        + " -c 'import time; time.sleep(3600)' \"$(cat " + CANARY + ")\""
    )
    machine.wait_for_unit("lab-probe-canary.service")
    machine.wait_until_succeeds(
        "grep -qx \"canary pid=$(systemctl show -p MainPID --value lab-probe-canary.service)\" "
        + ARGV_LOG,
        timeout=60,
    )

    # The sink grabs the keyboard before any request is typed.
    machine.succeed("install -m 0600 /dev/null " + SINK)
    machine.succeed(
        "systemd-run --unit=lab-probe-sink " + PYTHON + " " + "${sink}"
        + " " + SINK + " /run/lab-probe-test/sink.ready"
    )
    machine.wait_until_succeeds("test -s /run/lab-probe-test/sink.ready", timeout=60)

    # J1 and J2 through the installed command: a username and the password.
    machine.succeed(
        "(printf 'labuser '; cat " + PASSWORD + "; printf '\\n') | lab-probe-type | grep -qx 'ok 73'"
    )
    machine.wait_until_succeeds(sink_is(*USER_PASS), timeout=120)

    # A username alone types the fixed wrong password.
    machine.succeed("printf 'labuser\\n' | lab-probe-type | grep -qx 'ok 51'")
    machine.wait_until_succeeds(sink_is(*USER_PASS, *USER_FIXED), timeout=120)

    # Malformed requests get an error reply, exit 1, and type nothing.
    malformed = [
        ("printf 'LABUSER\\n'", "error bad username\n"),
        ("head -c 33 /dev/zero | tr '\\0' a; printf '\\n'", "error username too long\n"),
        (
            "printf 'labuser '; head -c 65 /dev/zero | tr '\\0' b; printf '\\n'",
            "error password too long\n",
        ),
        ("printf 'labuser extra field\\n'", "error too many fields\n"),
    ]
    for request, reason in malformed:
        status, out = machine.execute("(" + request + ") | lab-probe-type")
        assert status == 1 and out == reason, f"{request!r}: status {status}, reply {out!r}"

    # Non-root use is refused before the request is read.
    status, out = machine.execute(
        "printf 'labuser\\n' | setpriv --reuid=65534 --regid=65534 --clear-groups lab-probe-type"
    )
    assert status == 1 and out == "error not root\n", f"non-root: status {status}, reply {out!r}"

    # Requests the holder serves in order. Any text typed by a refused request
    # would reach the sink before this reply does.
    machine.succeed("printf 'labuser\\n' | lab-probe-type | grep -qx 'ok 51'")
    machine.wait_until_succeeds(sink_is(*USER_PASS, *USER_FIXED, *USER_FIXED), timeout=120)

    # The password is in no journal byte. The canary proves the scan can see a
    # match: it is logged, and the same scan finds it. The needle prints its
    # verdict and exits 0, because a driver command stops at its first nonzero
    # exit.
    machine.succeed("logger -t lab-probe-canary \"$(cat " + CANARY + ")\"")
    machine.wait_until_succeeds(
        "journalctl -b -o export > " + JOURNAL + " && " + PYTHON + " ${needle} "
        + CANARY + " < " + JOURNAL + " | grep -qx found",
        timeout=60,
    )
    verdict = machine.succeed(
        "journalctl -b -o export > " + JOURNAL + " && " + PYTHON + " ${needle} "
        + PASSWORD + " < " + JOURNAL + "; rm -f " + JOURNAL
    )
    assert verdict == "absent\n", f"password found in the journal: {verdict!r}"

    # No process argv carried the password through the run. The canary was seen
    # before the first request, and the monitor is still scanning now, so the
    # empty leak list covers every request.
    machine.succeed("systemctl is-active --quiet lab-probe-argv-monitor.service")
    machine.succeed("! grep -q '^leak pid=' " + ARGV_LOG)
  '';
}
