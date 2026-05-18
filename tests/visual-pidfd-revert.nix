# tests/visual-pidfd-revert.nix — Amendment A5.6 poll-only leader
# pidfd backstop gate.
#
# Proves the privilege-crossing fd path end to end on the REAL
# mechanism (splash + layer-shell greeter + real greetd full-auth →
# compositor relays to the privileged halmasuit-session broker → the
# broker forks-then-drops the session as an xdg_toplevel client):
#
#  1. ARMED: the broker `pidfd_open`s the session leader, SCM_RIGHTS-
#     sends a DUP to the compositor ON `SessionOpened` (worker→broker→
#     compositor, two hops, no raw pid on any frame), and the
#     compositor registers it as a POLL-ONLY calloop source —
#     observable as `session_leader_pidfd_armed`, emitted only on
#     successful end-to-end transfer + registration.
#  2. FIRED: when the session leader EXITS, the compositor's
#     independent pidfd dup becomes readable and drives the revert
#     (`session_leader_exited_via_pidfd`) WITHOUT the compositor ever
#     reaping/signalling it (the broker is the sole reaper — R9/A5).
#  3. REVERT: exactly one `foreground_changed {to: greeter}` after the
#     swap (the SwapGate makes whichever of {pidfd, SessionEnded,
#     client-disconnect} arrives later inert).
#  4. PID continuity (login-flash invariant) across the whole episode.
#
# The pidfd backstop is an ACCELERATOR, not the authoritative signal
# (that is `SessionEnded`, A5.6/HANDOFF §0.9.6); this gate proves it is
# wired and fires, not an ordering vs SessionEnded (the gate makes the
# later trigger inert — a legitimate race). Headless: PID/event stream
# + the session client's buffer attach work (the documented rendering
# gotcha only blanks scanned-out pixels). State-based throughout.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-splash,
  halmasuit-layer-shell-test-client,
  halmasuit-toplevel-test-client,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
  fixture = ./fixtures/splash-test.png;
  sessionCmd = pkgs.writeShellScript "halmasuit-test-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit
    export WAYLAND_DISPLAY=wayland-0
    export HALMASUIT_TESTCLIENT_COLOR=#FF22AA
    exec ${halmasuit-toplevel-test-client}/bin/halmasuit-toplevel-test-client
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-pidfd-revert";

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
        greeterCommand  = "${pkgs.writeShellScript "halmasuit-fg-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#2255FF
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

      systemd.services.test-splash = {
        description = "halmasuit A5.6 splash background";
        after = [ "halmasuit.service" ];
        serviceConfig = {
          User  = "halmasuit-greeter";
          Group = "halmasuit-greeter";
          ExecStart = "${pkgs.writeShellScript "fg-splash" ''
            export HALMASUIT_SPLASH_IMAGE=${fixture}
            exec ${halmasuit-splash}/bin/halmasuit-splash
          ''}";
          Environment = [
            "XDG_RUNTIME_DIR=/run/halmasuit"
            "WAYLAND_DISPLAY=wayland-0"
          ];
          Restart = "no";
        };
      };

      users.users.alice = {
        isNormalUser = true;
        uid          = 1001;
        group        = "alice";
        password     = "testpassword";
        extraGroups  = [ "halmasuit-greeter" ];
      };
      users.groups.alice.gid = 1001;

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

      environment.systemPackages = [ halmasuit-vm-client ];

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 2048;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import sys

    sys.path.insert(0, "${./lib}")
    import visual

    def events():
        return [e["event"] for e in visual.introspect_events(machine)]

    def index_of(name):
        evs = events()
        assert name in evs, f"expected event {name!r}; stream={evs}"
        return evs.index(name)

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.succeed("systemctl start test-splash.service")
    machine.wait_until_succeeds(
        "journalctl -u test-splash | grep -qF 'halmasuit-splash: presented'", timeout=90
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=30
    )

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd ${sessionCmd} "
        "--timeout 20"
    )

    # The swap fires (two-key) AND the leader pidfd is armed. Bare
    # tokens: journald backslash-escapes the inner tracing JSON, so
    # quoted/colon substrings never match (the documented gotcha).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF greeter_terminated"
        " && journalctl -u halmasuit -o cat | grep -qF session_leader_pidfd_armed",
        timeout=60,
    )

    # ── KEY ASSERTION 1: SCM_RIGHTS end-to-end ──────────────────────
    # `session_leader_pidfd_armed` is emitted ONLY when the leader
    # pidfd crossed worker→broker→compositor (two SCM_RIGHTS hops, no
    # raw pid on any frame) AND was registered as a poll-only calloop
    # source. Its presence is the end-to-end transfer proof. It must
    # come at/after SessionOpened (the frame it rides on) and the swap.
    i_open  = index_of("session_opened")
    i_armed = index_of("session_leader_pidfd_armed")
    i_swap  = next(
        n for n, e in enumerate(visual.introspect_events(machine))
        if e["event"] == "foreground_changed" and e["to"] == "session"
    )
    assert i_armed >= i_open, (
        f"pidfd armed before SessionOpened (it rides that frame): {events()}"
    )
    print("PASS: A5.6 leader pidfd crossed worker→broker→compositor and armed")

    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: {halmasuit_pid} -> {pid_now}"
    )

    # ── KEY ASSERTION 2: pidfd fires on leader exit, drives revert ───
    # Kill the leader-exec'd session client. The compositor's
    # INDEPENDENT pidfd dup becomes readable (`EPOLLIN` = task exited)
    # and `handle_leader_pidfd_ready` drives the revert — poll-only, it
    # never waitid/reaps/signals (the broker is the sole reaper, R9/A5).
    machine.succeed("pkill -KILL -f halmasuit-toplevel-test-client")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF session_leader_exited_via_pidfd",
        timeout=30,
    )
    evs = visual.introspect_events(machine)
    assert any(e["event"] == "session_leader_exited_via_pidfd" for e in evs), (
        f"the poll-only pidfd backstop must observe the leader exit: {events()}"
    )
    reverts_after_swap = [
        n for n, e in enumerate(evs)
        if e["event"] == "foreground_changed" and e["to"] == "greeter"
        and n > i_swap
    ]
    assert len(reverts_after_swap) == 1, (
        "expected EXACTLY ONE revert (foreground→greeter) after the "
        f"swap — the gate must make the 2nd trigger inert: got "
        f"{reverts_after_swap} in {events()}"
    )
    print("PASS: pidfd backstop fired on leader exit and drove exactly one revert")

    pid_final = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_final == halmasuit_pid, (
        f"halmasuit restarted across the revert: {halmasuit_pid} -> {pid_final}"
    )

    print("visual-pidfd-revert: ALL ASSERTIONS PASSED")
  '';
}
