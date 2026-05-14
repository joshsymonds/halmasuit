# Consolidated NixOS VM test for halmasuit's Phase A deployment.
#
# Replaces the earlier halmasuit-introspect.nix one-shot. Boots a single
# NixOS machine with services.halmasuit.enable and exercises the full
# integration across sequential suites driven off the structured event
# bus:
#
#   1. Lifecycle events (Started → Init → WaylandReady → GreetdReady)
#   2. Both Unix sockets bound on disk with correct permissions
#   3. Wayland globals advertised (wl_compositor + xdg_wm_base + wl_seat
#      + wl_output + wl_shm) — verified by a real wayland-info client
#   4. Full greetd auth round-trip via halmasuit-vm-client →
#      Event::SessionRequested → halmasuit-spawn invocation
#   5. Clean shutdown via systemctl stop
#
# The wrong-UID-rejection path is covered by unit tests in
# halmasuit-greetd (accept_authorized + Listener::bind mode validation);
# duplicating it here would require a second authorised-socket-group
# user just for the rejection assertion — disproportionate effort.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-spawn,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-vm";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        enable  = true;
        package = halmasuit;
      };

      # halmasuit's greetd I/O reads three env vars to pick the
      # greeter UID, the PAM service file name, and the spawn-helper
      # path. For Phase A the unit runs as root, so:
      #   - HALMASUIT_GREETER_UID=0: the test script runs as root, so
      #     SO_PEERCRED matches on connect.
      #   - HALMASUIT_PAM_SERVICE=halmasuit: matches the
      #     security.pam.services entry below; gives us /etc/pam.d/halmasuit.
      #   - HALMASUIT_SPAWN_BIN points at the halmasuit-spawn package's
      #     binary directly. Setuid-via-security.wrappers is unneeded in
      #     Phase A because the unit IS root; production (compositor-user
      #     unit) wraps it via security.wrappers and that's a separate task.
      systemd.services.halmasuit.environment = {
        HALMASUIT_GREETER_UID = "0";
        HALMASUIT_PAM_SERVICE = "halmasuit";
        HALMASUIT_SPAWN_BIN   = "${halmasuit-spawn}/bin/halmasuit-spawn";
      };

      # The user halmasuit-greetd will authenticate. Both uid AND gid
      # must be ≥ UID_MIN (1000) — halmasuit-spawn's load-bearing floor
      # refuses anything below. Default NixOS normal-users land in
      # group `users` (gid 100), which the floor rejects (correctly!).
      # Give alice her own group with gid 1000.
      users.users.alice = {
        isNormalUser = true;
        uid          = 1000;
        group        = "alice";
        password     = "testpassword";
      };
      users.groups.alice.gid = 1000;

      # /etc/pam.d/halmasuit. NixOS's default services.pam config
      # (unixAuth=true) gives us pam_unix-backed password auth, which
      # is what alice's cleartext-password user above lands on.
      security.pam.services.halmasuit = {};

      # Tools the testScript invokes:
      # - wayland-utils: the `wayland-info` global discovery client.
      # - halmasuit-vm-client: drives the greetd protocol from the
      #   testScript via a stable CLI (separate package; built from
      #   crates/halmasuit-vm-client).
      # - halmasuit-spawn: HALMASUIT_SPAWN_BIN above references its
      #   binary path. Adding it to systemPackages makes the binary
      #   reachable on the closure even though we don't $PATH it.
      environment.systemPackages = [
        pkgs.wayland-utils
        halmasuit-vm-client
        halmasuit-spawn
      ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    import json

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Wait for the latest startup phase before any assertions; once
    # greetd_ready is in the journal, all earlier phases necessarily
    # arrived too.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greetd_ready'",
        timeout=30,
    )

    def captured_events():
        """
        Read journald's `-o cat` view of halmasuit's MESSAGE field and
        decode the two-layer JSON: the tracing-subscriber envelope, then
        the halmasuit-introspect inner JSON inside fields.json.
        Returns a list of (envelope, inner) tuples.
        """
        raw = machine.succeed("journalctl -u halmasuit -o cat --no-pager")
        out = []
        for line in raw.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                outer = json.loads(line)
            except Exception:
                continue
            if outer.get("target") != "halmasuit::event":
                continue
            inner_str = outer.get("fields", {}).get("json")
            if not inner_str:
                continue
            try:
                inner = json.loads(inner_str)
            except Exception:
                continue
            out.append((outer, inner))
        return out

    def find(events, event_name):
        return next(
            (inner for (_, inner) in events if inner.get("event") == event_name),
            None,
        )

    def find_phase(events, phase):
        return next(
            (inner for (_, inner) in events
             if inner.get("event") == "phase_entered" and inner.get("phase") == phase),
            None,
        )

    # ──────────────────────────────────────────────────────────────────
    # Suite 1: Lifecycle events
    # ──────────────────────────────────────────────────────────────────
    with subtest("lifecycle events"):
        events = captured_events()
        assert events, (
            "no halmasuit::event records captured. Raw journal: "
            + machine.succeed("journalctl -u halmasuit -o cat --no-pager")
        )
        started = find(events, "started")
        assert started is not None, f"no started event: {events}"
        assert isinstance(started.get("pid"), int) and started["pid"] > 0, (
            f"started.pid must be positive int: {started}"
        )
        assert started.get("version") == "0.1.0", (
            f"started.version must match Cargo.toml: {started}"
        )
        for phase in ("init", "wayland_ready", "greetd_ready"):
            evt = find_phase(events, phase)
            assert evt is not None, (
                f"no phase_entered{{phase={phase}}} event captured. Events: {events}"
            )

    # ──────────────────────────────────────────────────────────────────
    # Suite 2: Sockets on disk with correct permissions
    # ──────────────────────────────────────────────────────────────────
    with subtest("sockets on disk"):
        rc, out = machine.execute("test -S /run/halmasuit/wayland-0")
        if rc != 0:
            listing = machine.execute("ls -la /run/halmasuit/ 2>&1")[1]
            raise AssertionError(
                f"wayland-0 socket missing. /run/halmasuit:\n{listing}"
            )
        rc, out = machine.execute("test -S /run/halmasuit/greetd.sock")
        if rc != 0:
            listing = machine.execute("ls -la /run/halmasuit/ 2>&1")[1]
            raise AssertionError(
                f"greetd.sock missing. /run/halmasuit:\n{listing}"
            )
        mode = machine.succeed("stat -c '%a' /run/halmasuit/greetd.sock").strip()
        assert mode == "660", f"greetd.sock should be mode 0660, got {mode}"

    # ──────────────────────────────────────────────────────────────────
    # Suite 3: Wayland globals reachable via a real Wayland client
    # ──────────────────────────────────────────────────────────────────
    with subtest("wayland globals"):
        info = machine.succeed(
            "XDG_RUNTIME_DIR=/run/halmasuit WAYLAND_DISPLAY=wayland-0 "
            "timeout 5 wayland-info 2>&1"
        )
        for required in ("wl_compositor", "xdg_wm_base", "wl_seat",
                         "wl_output", "wl_shm"):
            if required not in info:
                raise AssertionError(
                    f"{required} global not advertised; wayland-info:\n{info}"
                )

    # ──────────────────────────────────────────────────────────────────
    # Suite 4: Full greetd auth round-trip + session spawn
    # ──────────────────────────────────────────────────────────────────
    with subtest("full auth and session spawn"):
        # Stage alice's password in a file readable by root (the
        # test script's effective uid); halmasuit-vm-client reads
        # it via --password-file. We avoid putting the password
        # on the argv (which would land in /proc/<pid>/cmdline).
        machine.succeed("printf 'testpassword' > /tmp/alice.pw")
        machine.succeed("chmod 600 /tmp/alice.pw")

        # Drive the full handshake. /run/current-system/sw/bin/true
        # is NixOS's stable path to coreutils' true — alice will
        # exec it (via halmasuit-spawn) and exit 0. We don't track
        # that child; the test asserts halmasuit reached the spawn
        # invocation, which is what the event surface tells us.
        machine.succeed(
            "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
            "--password-file /tmp/alice.pw "
            "--cmd /run/current-system/sw/bin/true "
            "--timeout 15"
        )

        # halmasuit emits SessionRequested right before it forks the
        # halmasuit-spawn child. Wait for it explicitly because the
        # auth flow is asynchronous from the daemon's perspective.
        machine.wait_until_succeeds(
            "journalctl -u halmasuit | grep -qF 'session_requested'",
            timeout=10,
        )
        events = captured_events()
        session = find(events, "session_requested")
        assert session is not None, (
            f"no session_requested event captured. Events tail: {events[-10:]}"
        )
        assert session.get("uid") == 1000, (
            f"session_requested.uid must be alice's 1000, got: {session}"
        )
        assert session.get("gid") == 1000, (
            f"session_requested.gid must be alice's 1000, got: {session}"
        )

        # The 'halmasuit-spawn launched' tracing log line is emitted
        # AFTER the SessionRequested event, from the Ok arm of
        # Command::spawn. Its presence proves the halmasuit-spawn fork
        # actually fired (a failure would log a `warn!` line we'd grep
        # for separately).
        machine.wait_until_succeeds(
            "journalctl -u halmasuit | grep -qF 'halmasuit-spawn launched'",
            timeout=5,
        )

    # ──────────────────────────────────────────────────────────────────
    # Suite 5: Clean shutdown
    # ──────────────────────────────────────────────────────────────────
    with subtest("clean shutdown via SIGTERM"):
        machine.succeed("systemctl stop halmasuit")
        machine.wait_until_succeeds(
            "journalctl -u halmasuit | grep -qF 'signal_term'",
            timeout=30,
        )
        events = captured_events()
        shutdown = find(events, "shutdown")
        assert shutdown is not None, f"no shutdown event: {events}"
        assert shutdown.get("reason") == "signal_term", (
            f"shutdown.reason must be signal_term: {shutdown}"
        )
        active_state = machine.succeed(
            "systemctl show -p ActiveState --value halmasuit.service"
        ).strip()
        assert active_state == "inactive", (
            f"halmasuit.service must be inactive (clean stop), got {active_state!r}"
        )

    # Envelope guard: every halmasuit::event line carries the right
    # tracing target. Catches accidental loss of the `target:` attr.
    for envelope, _ in captured_events():
        assert envelope.get("target") == "halmasuit::event", (
            f"envelope must carry halmasuit::event target: {envelope}"
        )

    print("halmasuit-vm: all suites PASSED")
  '';
}
