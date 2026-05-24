# tests/visual-revert.nix — Amendment A5 two-key swap + revert gate.
#
# Proves the load-bearing flash-free property on the REAL mechanism
# (halmasuit's internal wallpaper plane + layer-shell greeter + real
# greetd full-auth → the compositor relays to the privileged
# halmasuit-session broker →
# the broker forks-then-drops the session as an xdg_toplevel client):
#
#  1. TWO-KEY: the VISIBLE greeter→session swap
#     (`foreground_changed {to: session}`) fires ONLY after BOTH
#     `session_opened` (key 1, the broker frame) AND
#     `session_client_first_frame` (key 2, the session wl_client's
#     first non-empty buffer) — never on `session_opened` alone
#     (that is the exact flash this project deletes). Asserted by
#     event ORDERING in halmasuit's introspect stream.
#  2. REVERT: when the session ends (the leader is killed → the broker,
#     sole reaper, sends `SessionEnded`), the compositor reverts the
#     foreground to the greeter (`foreground_changed
#     {to: greeter}`) — A5.5.
#  3. PID continuity across BOTH transitions (login-flash invariant on
#     the real path): halmasuit never restarts.
#
# Headless (virtio-gpu-pci): per the documented rendering gotcha the
# scanned-out pixels are black, but the PID/event stream and the
# session client's buffer attach (key 2 fires on `buffer_size`, not on
# scanned-out luma) work — which is exactly what the two-key ORDERING
# proof needs. The pixel-level no-black-frame proof on this path is
# visual-foreground.nix's job (virtio-vga-gl); this gate is the
# sequencing + revert proof. State-based throughout (no time.sleep).

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
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
  name = "visual-revert";

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
        wallpaper = { type = "image"; source = fixture; };
        greeterCommand  = "${pkgs.writeShellScript "halmasuit-fg-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#2255FF
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
      };

      # The authenticated session user (same shape as
      # visual-foreground.nix: own gid-1001 group to clear the broker's
      # ≥1000 UID/GID floor; test-local halmasuit-greeter membership so
      # the session she spawns can open the 0660 wayland socket).
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
    # The wallpaper plane is composited internally from frame 0.
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

    # Drive a REAL greetd full-auth as the greeter uid.
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

    # The session toplevel maps and the two-key swap fires.
    # `greeter_terminated` is emitted ONLY by the swap, synchronously
    # right after BOTH keys are in — its presence is the unambiguous
    # "swap happened" signal. Bare token: journald backslash-escapes
    # the inner tracing JSON, so quoted/colon substrings never match
    # (the documented escaping gotcha) — grep a robust word instead.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF session_client_first_frame"
        " && journalctl -u halmasuit -o cat | grep -qF greeter_terminated",
        timeout=60,
    )

    # ── KEY ASSERTION 1: two-key ordering ───────────────────────────
    # The visible swap (foreground→session) must come AFTER the session
    # client's first non-empty frame AND after SessionOpened. Swapping
    # on SessionOpened alone (before the session has painted) is the
    # flash this project exists to delete.
    i_req   = index_of("session_requested")
    i_open  = index_of("session_opened")
    i_frame = index_of("session_client_first_frame")
    evs = visual.introspect_events(machine)
    fg = [e for e in evs if e["event"] == "foreground_changed"]
    # foreground order is greeter (startup) then session (the swap).
    fg_to = [e["to"] for e in fg]
    assert fg_to[:2] == ["greeter", "session"], f"foreground order wrong: {fg_to}"
    i_swap = next(
        n for n, e in enumerate(evs)
        if e["event"] == "foreground_changed" and e["to"] == "session"
    )
    assert i_req < i_open, f"SessionRequested must precede SessionOpened: {events()}"
    assert i_swap > i_open, (
        f"swap fired before/without SessionOpened (key 1): {events()}"
    )
    assert i_swap > i_frame, (
        "swap fired before the session client's first frame (key 2) — "
        f"this is the flash: {events()}"
    )
    print("PASS: two-key — swap waited for SessionOpened AND first session frame")

    # halmasuit did NOT restart across the swap.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"halmasuit restarted across the swap: {halmasuit_pid} -> {pid_now}"
    )

    # ── KEY ASSERTION 2: revert ─────────────────────────────────────
    # End the session: kill the leader-exec'd session client. Two
    # revert triggers race (A5.5 "SessionEnded frame OR session-client
    # disconnect — whichever first"): the wl_client disconnect
    # (observed by the compositor directly) typically beats the broker
    # frame (broker must waitpid the leader, then send). The SwapGate
    # makes the SECOND trigger inert → EXACTLY ONE revert. Both signals
    # must still appear; we do NOT assert an order between them (the
    # race is legitimate spec behaviour).
    machine.succeed("pkill -KILL -f halmasuit-toplevel-test-client")
    # `session_ended` is the broker-frame signal; its bare token
    # (escaped-JSON gotcha) appearing means the broker reaped + sent
    # the outcome. The revert `foreground_changed {to: greeter}` is
    # emitted no later than this (the disconnect trigger usually fires
    # earlier — see the event stream).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF session_ended",
        timeout=30,
    )
    evs = visual.introspect_events(machine)
    assert any(e["event"] == "session_ended" for e in evs), (
        f"broker must still report SessionEnded: {events()}"
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
    print("PASS: revert — exactly one foreground→greeter after the session ended")

    # PID still continuous across the revert (no compositor restart on
    # session end either).
    pid_final = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_final == halmasuit_pid, (
        f"halmasuit restarted across the revert: {halmasuit_pid} -> {pid_final}"
    )

    print("visual-revert: ALL ASSERTIONS PASSED")
  '';
}
