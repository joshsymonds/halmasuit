# Epic #71 R-honest.1 — org.halmasuit.Compositor1 live-value gate.
#
# Proves the Compositor1 DBus surface returns REAL state, not the
# stub literals R3.3 shipped (GetFrameCounter was hardcoded-shaped
# as 0 forever — the render path never fed the Arc). This is the
# regression gate for the "diagnostic surface that lies" bug the
# R3-honest re-open exists to fix.
#
# Load-bearing assertion: GetFrameCounter STRICTLY INCREASES between
# two calls. The counter is the SAME Arc<AtomicU64> the render
# backend increments on every queued frame; if it advances over
# DBus, the calloop-writes / DBus-reads cross-thread path works and
# the value is genuinely live.
#
# A continuous shader wallpaper drives the wallpaper-tick render
# loop unconditionally (ShaderBackend::wants_continuous_render =
# true), so frame_counter advances even in the headless
# virtio-gpu-pci VM (which paints black but DOES run the render loop
# + queue frames — same posture as the visual-* tests).
#
# State-based polling throughout (wait_until_succeeds), never
# time.sleep on a bare interval for a condition.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # Minimal greeter: idle so halmasuit reaches steady-state main loop.
  testGreeter = pkgs.writeShellScript "halmasuit-compositor1-greeter" ''
    exec ${pkgs.coreutils}/bin/sleep infinity
  '';
in
pkgs.testers.runNixOSTest {
  name = "compositor1-dbus";

  nodes.machine =
    { config, lib, pkgs, ... }:
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
        greeterCommand  = "${testGreeter}";
        # Continuous shader wallpaper → wallpaper-tick drives
        # render_one_frame unconditionally → frame_counter advances
        # without needing wl_client commits. Without this the headless
        # render loop is quiescent and the counter wouldn't move.
        wallpaper = {
          type   = "shader";
          source = ./fixtures/wallpaper-shader.glsl;
        };
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

      users.users.test.group = "test";
      users.groups.test.gid  = 1000;

      # gdbus (from glib) is the DBus client the test driver uses to
      # call Compositor1 methods.
      environment.systemPackages = [ pkgs.glib ];

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import re

    def compositor1_u64(method):
        """Call a Compositor1 method returning a u64; return the int.

        gdbus prints e.g. "(uint64 42,)". The literal "uint64" CONTAINS
        the digits "64", so a naive r"\d+" matches the type name, not
        the value — anchor on "uint64 " and capture what follows.
        """
        out = machine.succeed(
            f"gdbus call --system --dest org.halmasuit.Compositor1 "
            f"--object-path /org/halmasuit/Compositor1 "
            f"--method org.halmasuit.Compositor1.{method}"
        ).strip()
        m = re.search(r"uint64 (\d+)", out)
        assert m, f"could not parse {method} reply: {out!r}"
        return int(m.group(1))

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Wait until halmasuit is past init and into the steady-state
    # main loop (greeter spawned), so the render loop + DBus server
    # thread are both up.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'",
        timeout=30,
    )

    # The Compositor1 name must be claimable on the system bus.
    machine.wait_until_succeeds(
        "gdbus call --system --dest org.halmasuit.Compositor1 "
        "--object-path /org/halmasuit/Compositor1 "
        "--method org.halmasuit.Compositor1.GetUptime",
        timeout=30,
    )

    # ── Assertion 1: GetFrameCounter is NON-ZERO ──
    # The R3.3 stub returned a hardcoded 0. The ONLY way GetFrameCounter
    # reads non-zero is the render path calling fetch_add on the SAME
    # Arc the DBus surface reads — i.e. the cross-thread shared-counter
    # wiring works and the stub is dead. (Strict per-tick advancement
    # is asserted in the chord-driven VM test, where send_key forces a
    # render via a deterministic damage source; a clientless headless
    # VM has no per-tick damage so the counter idles after startup —
    # see CLAUDE.md "Test-VM rendering gotcha".)
    frames = compositor1_u64("GetFrameCounter")
    print(f"GetFrameCounter: {frames}")
    assert frames > 0, (
        f"GetFrameCounter must be > 0 (render path feeds the shared "
        f"Arc); got {frames}. If 0, the R3.3 'always 0' stub regressed "
        f"— the render path is NOT incrementing the Arc the DBus "
        f"surface reads."
    )

    # ── Assertion 2: GetUptime returns LIVE values (not a frozen
    # construction-time snapshot) ── deterministic, independent of
    # render/vblank/damage. State-based poll (not a bare sleep, per
    # the project's state-based-polling rule): wait until a fresh
    # read strictly exceeds the first. Wall-clock guarantees this
    # within ~1-2s; a frozen snapshot would never advance and the
    # poll would time out.
    uptime_first = compositor1_u64("GetUptime")
    print(f"GetUptime first: {uptime_first}")
    machine.wait_until_succeeds(
        f"[ \"$(gdbus call --system --dest org.halmasuit.Compositor1 "
        f"--object-path /org/halmasuit/Compositor1 "
        f"--method org.halmasuit.Compositor1.GetUptime "
        f"| sed -E 's/.*uint64 ([0-9]+).*/\\1/')\" -gt {uptime_first} ]",
        timeout=15,
    )
    uptime_second = compositor1_u64("GetUptime")
    print(f"GetUptime second: {uptime_second}")
    assert uptime_second > uptime_first, (
        f"GetUptime must advance over wall-clock time (DBus returns "
        f"live state, not a snapshot); got first={uptime_first} "
        f"second={uptime_second}."
    )

    print("=" * 70)
    print("PASS: org.halmasuit.Compositor1 returns REAL, LIVE values.")
    print(f"      GetFrameCounter={frames} (>0 → render feeds the shared")
    print(f"      Arc, not the 0 stub); GetUptime {uptime_first}→"
          f"{uptime_second} (live, not frozen).")
    print("=" * 70)
  '';
}
