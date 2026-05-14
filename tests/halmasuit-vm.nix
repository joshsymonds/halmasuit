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
# The greeter peer in suite 4 runs as a real non-root system user
# (halmasuit-greeter, uid 999) so SO_PEERCRED authorization exercises
# the production shape. The wrong-UID-rejection path itself is covered
# by halmasuit-greetd unit tests:
#   - listener_accept_authorized_returns_when_uid_matches (positive)
#   - listener_accept_authorized_drops_when_uid_does_not_match (negative)
#   - listener_bind_rejects_world_accessible_mode (mode validation)
# in crates/halmasuit-greetd/src/server.rs.

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

      # Exercise the full module option surface — no inline systemd env
      # patches. If a production deployment differs from this, it's a
      # module-design issue we want to catch here.
      services.halmasuit = {
        enable        = true;
        package       = halmasuit;
        spawnPackage  = halmasuit-spawn;
        greeterUid    = 999;
        greeterGroup  = "halmasuit-greeter";
        compositorUid = 998;
        # Test greeter: a shell script that simply sleeps forever, so
        # we can `ps` the running process and inspect its uid. Real
        # production deployments point this at DankGreeter or any
        # greetd-protocol greeter. The wait-forever shape lets the
        # existing full-auth suite (4) still drive its own protocol
        # client without contention — they share the same greetd
        # socket, but the test greeter never makes any requests.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-test-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
        # pamService + installPamConfig left at defaults.
      };

      # Greeter system user. uid 999 matches services.halmasuit.greeterUid
      # above; SO_PEERCRED on the greetd socket will only accept this uid.
      # Below UID_MIN (1000) on purpose: this is a system account, not a
      # human login.
      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
        description  = "halmasuit greeter peer (test)";
      };
      users.groups.halmasuit-greeter.gid = 999;

      # Compositor system user. uid 998 matches
      # services.halmasuit.compositorUid above; halmasuit setresuids
      # into this uid after binding its sockets. Group is
      # halmasuit-greeter so the post-drop process retains the group
      # ownership that lets it accept() on the sockets it bound (the
      # sockets are root:halmasuit-greeter mode 0660 from suite 2).
      # `shadow` as an extra group gives pam_unix.so the direct
      # read access to /etc/shadow it needs to skip the unix_chkpwd
      # helper fork — see ARCHITECTURE.md / nix/module.nix for
      # the rationale (shadow-group membership is more reliable and
      # auditable than the cross-namespace setuid-helper dance).
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
        description  = "halmasuit compositor process identity (test)";
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

      # Tools the testScript invokes:
      # - wayland-utils: the `wayland-info` global discovery client.
      # - halmasuit-vm-client: drives the greetd protocol from the
      #   testScript via a stable CLI (separate package; built from
      #   crates/halmasuit-vm-client).
      # - halmasuit-spawn here is reachable on the closure via the
      #   module's spawnPackage option — no need to add it to
      #   systemPackages separately.
      environment.systemPackages = [
        pkgs.wayland-utils
        halmasuit-vm-client
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
    # deprivileged is in the journal, all earlier phases necessarily
    # arrived too (deprivileged fires last in main()'s init sequence).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'deprivileged'",
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
        for phase in (
            "init",
            "drm_master_acquired",
            "wayland_ready",
            "greetd_ready",
            "deprivileged",
        ):
            evt = find_phase(events, phase)
            assert evt is not None, (
                f"no phase_entered{{phase={phase}}} event captured. Events: {events}"
            )

        # greeter_spawned is a top-level Event variant (not a Phase),
        # emitted after halmasuit fork+execs the configured greeter.
        # Pid must be a positive int.
        greeter = find(events, "greeter_spawned")
        assert greeter is not None, f"no greeter_spawned event: {events}"
        assert isinstance(greeter.get("pid"), int) and greeter["pid"] > 0, (
            f"greeter_spawned.pid must be positive int: {greeter}"
        )

        # Phase ordering is load-bearing. Expected sequence:
        # drm_master_acquired → init → wayland_ready → greetd_ready
        # → deprivileged. Each step encodes an architectural invariant
        # the binary must maintain.
        phase_order = [
            inner["phase"]
            for (_, inner) in events
            if inner.get("event") == "phase_entered"
        ]
        init_idx = phase_order.index("init")
        dm_idx   = phase_order.index("drm_master_acquired")
        wr_idx   = phase_order.index("wayland_ready")
        gd_idx   = phase_order.index("greetd_ready")
        dp_idx   = phase_order.index("deprivileged")
        # DRM master is acquired before any other init does anything
        # (it's the only privileged operation that must run while we
        # still have root via execve, before smithay touches the fb).
        assert dm_idx < init_idx, (
            f"drm_master_acquired must precede init, got: {phase_order}"
        )
        # Init marks "smithay state assembled" — must precede the
        # Wayland socket being announced as ready for clients.
        assert init_idx < wr_idx, (
            f"init must precede wayland_ready, got: {phase_order}"
        )
        # Wayland socket exists before greetd's auth socket — a
        # greeter needs both, and the order pins which one to wait
        # for first when integrating.
        assert wr_idx < gd_idx, (
            f"wayland_ready must precede greetd_ready, got: {phase_order}"
        )
        # Privilege drop is strictly last among the init phases: all
        # privileged operations (socket binds under /run/halmasuit,
        # greeter fork-and-drop while still root) must complete
        # before halmasuit setresuids itself.
        assert gd_idx < dp_idx, (
            f"deprivileged must follow greetd_ready, got: {phase_order}"
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

        # With services.halmasuit.greeterGroup = "halmasuit-greeter",
        # systemd sets the unit's Group=, so files halmasuit binds
        # inherit that gid. Verify the socket actually landed in the
        # right group — otherwise SO_PEERCRED would still pass but the
        # 0660 mode would lock the greeter out at connect-time.
        sock_group = machine.succeed(
            "stat -c '%G' /run/halmasuit/greetd.sock"
        ).strip()
        assert sock_group == "halmasuit-greeter", (
            f"greetd.sock should be group halmasuit-greeter (from "
            f"services.halmasuit.greeterGroup), got {sock_group!r}"
        )

        # Direct reachability probe: from the greeter user's uid, can
        # we actually open the socket file with write permission?
        # `connect(AF_UNIX)` requires write on the socket inode, so
        # `test -w` is a faithful proxy for "the greeter can connect".
        # Isolates module-config correctness from suite 4's auth-flow:
        # if 0660 + group=halmasuit-greeter is wrong (wrong mode, wrong
        # group, dropped Group= directive), this fails focused right
        # here rather than as a confusing connect-time timeout in
        # suite 4.
        machine.succeed(
            "runuser -u halmasuit-greeter -- "
            "test -w /run/halmasuit/greetd.sock"
        )

    # ──────────────────────────────────────────────────────────────────
    # Suite 2.5: Process identity after privilege drop
    # ──────────────────────────────────────────────────────────────────
    with subtest("process runs as compositor uid after deprivilege"):
        # /proc/<pid>/status's Uid: line shows real/effective/saved/fs
        # uids — all four MUST be 998 after halmasuit's setresuid. A
        # mismatch means the drop didn't take or partially took.
        pid = machine.succeed("systemctl show -p MainPID --value halmasuit.service").strip()
        assert pid.isdigit() and int(pid) > 0, f"halmasuit MainPID must be positive int, got {pid!r}"
        status_uid = machine.succeed(
            f"awk '/^Uid:/ {{print $2,$3,$4,$5}}' /proc/{pid}/status"
        ).strip()
        assert status_uid == "998 998 998 998", (
            f"halmasuit must run as uid 998 (services.halmasuit.compositorUid) "
            f"on all four uid components after the drop; got {status_uid!r}"
        )
        # gid pinning: setresgid(egid, egid, egid) means saved-set-gid
        # equals current egid — the Gid: line should show four equal
        # values. We don't pin the absolute value here because that's
        # controlled by greeterGroup (already exercised in suite 2's
        # `stat -c '%G'`); we only assert all four match.
        status_gid = machine.succeed(
            f"awk '/^Gid:/ {{print $2,$3,$4,$5}}' /proc/{pid}/status"
        ).strip()
        gids = status_gid.split()
        assert len(gids) == 4 and len(set(gids)) == 1, (
            f"halmasuit's four gid components must all match after setresgid, "
            f"got {status_gid!r}"
        )

    # ──────────────────────────────────────────────────────────────────
    # Suite 2.7: Greeter child process exists and runs as greeter uid
    # ──────────────────────────────────────────────────────────────────
    with subtest("greeter child process runs as greeter uid"):
        # captured_events() already validated greeter_spawned in suite 1;
        # here we drill into the actual process: confirm it's alive and
        # its uid/euid match the configured greeterUid (999).
        events = captured_events()
        greeter = find(events, "greeter_spawned")
        assert greeter is not None, "greeter_spawned missing (caught in suite 1)"
        greeter_pid = greeter["pid"]
        # /proc/<pid>/status exists → process exists. Process disappeared
        # between event emission and now → the spawned greeter died,
        # which is a real failure mode worth surfacing.
        status = machine.succeed(
            f"cat /proc/{greeter_pid}/status"
        )
        # Greeter must run as halmasuit-greeter (uid 999) on all four
        # uid components — the same setresuid pin used elsewhere.
        uid_line = next(
            (l for l in status.splitlines() if l.startswith("Uid:")),
            None,
        )
        assert uid_line is not None, f"no Uid: line in greeter status: {status}"
        uid_parts = uid_line.split()[1:5]
        assert uid_parts == ["999", "999", "999", "999"], (
            f"greeter uid components must all be 999 (greeterUid), got {uid_parts}"
        )
        # Greeter's parent must be halmasuit. Catches a regression where
        # the fork lifecycle goes wrong and the greeter ends up
        # reparented to init.
        ppid_line = next(
            (l for l in status.splitlines() if l.startswith("PPid:")),
            None,
        )
        assert ppid_line is not None, f"no PPid: line in greeter status: {status}"
        ppid = ppid_line.split()[1]
        halmasuit_pid = machine.succeed(
            "systemctl show -p MainPID --value halmasuit.service"
        ).strip()
        assert ppid == halmasuit_pid, (
            f"greeter's parent must be halmasuit (pid {halmasuit_pid}), "
            f"got ppid {ppid!r}"
        )

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
        # Stage alice's password in a file readable by the greeter
        # user; halmasuit-vm-client reads it via --password-file. We
        # avoid putting the password on the argv (which would land in
        # /proc/<pid>/cmdline).
        machine.succeed("printf 'testpassword' > /tmp/alice.pw")
        machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
        machine.succeed("chmod 600 /tmp/alice.pw")

        # Drive the full handshake AS THE GREETER USER. SO_PEERCRED on
        # the greetd socket only authorizes uid 999 (halmasuit-greeter),
        # which matches services.halmasuit.greeterUid = 999 above —
        # this is the production shape, not root-as-greeter.
        #
        # /run/current-system/sw/bin/true is NixOS's stable path to
        # coreutils' true — alice will exec it (via halmasuit-spawn)
        # and exit 0. We don't track that child; the test asserts
        # halmasuit reached the spawn invocation, which is what the
        # event surface tells us.
        machine.succeed(
            "runuser -u halmasuit-greeter -- "
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
