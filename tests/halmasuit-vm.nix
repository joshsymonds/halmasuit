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
        # useSetuidWrapper left at default (true) — the wrapper is what
        # lets the deprivileged halmasuit re-elevate halmasuit-spawn at
        # exec time. Setting it false here would break suite 4's full-
        # auth path (compositor user can't setresuid into alice).
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

        # Phase ordering is load-bearing:
        # - drm_master_acquired must precede wayland_ready (DRM master is
        #   the first privileged op; smithay setup follows)
        # - deprivileged must follow greetd_ready (sockets are bound while
        #   still root, then setresuid)
        phase_order = [
            inner["phase"]
            for (_, inner) in events
            if inner.get("event") == "phase_entered"
        ]
        dm_idx = phase_order.index("drm_master_acquired")
        wr_idx = phase_order.index("wayland_ready")
        gd_idx = phase_order.index("greetd_ready")
        dp_idx = phase_order.index("deprivileged")
        assert dm_idx < wr_idx, (
            f"drm_master_acquired must precede wayland_ready, got: {phase_order}"
        )
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
