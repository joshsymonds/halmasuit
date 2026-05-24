# tests/visual-wallpaper-video.nix — Epic #12 task 10 hard gate.
#
# Validates the sandboxed video-decoder subsystem end-to-end against
# the real halmasuit-decoder binary, a real h264 file, real rsmpeg,
# and real namespace-based sandboxing. NO mocks anywhere (per the
# epic anti-pattern: "NO mocking video decode in VM tests. Real h264
# file, real rsmpeg, real sandbox. Mocks here are theatrically
# green.").
#
# Four orthogonal gates, all live in one boot:
#
#   1. Initial spawn — halmasuit-decoder appears under the compositor
#      uid (998 / halmasuit-compositor) within seconds of halmasuit
#      reaching `scanout_active`, proving DecoderRelay::spawn's
#      fork-exec + SCM_RIGHTS + LoadFile path executes end-to-end.
#
#   2. Crash recovery within budget — SIGKILL the decoder; the relay
#      detects the IPC EOF on its next poll and respawns in-place
#      under the 3-restarts-in-10s budget. We assert the new pid is
#      distinct from the killed pid.
#
#   3. Budget exhaustion — kill the decoder 3 more times (total 4
#      kills). The 4th failure pushes the relay's restart_history
#      past MAX_RESTARTS_PER_WINDOW; the relay marks itself dead
#      and stops respawning. We assert via the journal log
#      "decoder restart budget exhausted" AND by `pgrep` returning
#      nothing — and crucially, halmasuit.service stays ActiveState
#      = active (a buggy video must NOT take down the compositor;
#      that's the entire point of the sandbox subsystem).
#
#   4. login-flash continuity under video wallpaper — drive a real
#      greeter→session transition (test user, real pam_unix, real
#      broker) and assert halmasuit's MainPID is identical on both
#      sides. The video wallpaper must not perturb the no-flash
#      invariant. This is the canonical Epic R3 gate, asserted
#      under the new configuration.
#
# Test fixture: a 2-second 320x240 30fps h264 mp4 (libx264 YUV420p),
# generated at build time via pkgs.ffmpeg (GPL build — fixture is a
# build artifact only; halmasuit-decoder runtime-links ffmpeg-
# headless which is LGPL).
#
# The 2s duration is deliberately SHORT: short enough that the
# decoder hits EOF (and therefore the loop path) within the test
# window. Looping is a hard epic requirement (#2 "Loop playback").
# The mmap + custom-AVIO design (task #24) means the decoder
# rebuilds its AVFormatContext + AVIO over the SAME mmap on EOF —
# no fd-position state, no stuck AVIO EOF flag. The loop gate
# below counts wire-protocol frames sent and asserts the count
# grows past one decode pass (~60 frames at 30fps × 2s) by a
# margin that proves at least one full loop iteration delivered
# frames.
#
# Note on headless rendering (CLAUDE.md "Test-VM rendering gotcha"):
# this test does NOT screenshot — virtio-gpu-pci in CI paints solid
# black on scanout. All assertions are process-level (pgrep, journal,
# MainPID). Decode itself is pure software (libavcodec h264 decoder,
# Mesa llvmpipe for the texture upload) and works fine headless.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-decoder,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # 2s, 320x240, 30fps, baseline-profile h264 in mp4 — short enough
  # that the loop-on-EOF path fires during the test window (see
  # fixture-duration rationale above).
  videoFixture = pkgs.runCommand "halmasuit-test-wallpaper.mp4" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'testsrc=duration=2:size=320x240:rate=30' \
      -c:v libx264 -pix_fmt yuv420p -profile:v baseline -tune zerolatency \
      -movflags +faststart \
      $out
    # Sanity: the file must exist and be non-zero.
    test -s $out
  '';

  # Fallback image (PNG) the WallpaperEngine should swap in once the
  # decoder relay's restart budget exhausts. Solid blue so a future
  # visual-frame-capture gate can pixel-distinguish the swap; today
  # the test asserts via the log line emitted by
  # `WallpaperEngine::maybe_swap_fallback`.
  fallbackFixture = pkgs.runCommand "halmasuit-test-fallback.png" {
    nativeBuildInputs = [ pkgs.ffmpeg ];
  } ''
    ffmpeg -y -hide_banner -loglevel error \
      -f lavfi -i 'color=c=blue:s=320x240:d=1' \
      -frames:v 1 \
      $out
    test -s $out
  '';

  # Auth driver — same shape as login-flash.nix's: connects to the
  # GREETD_SOCK halmasuit exports to its greeter child, drives auth,
  # starts a sleep-infinity session, then idles so halmasuit's
  # greeter slot stays occupied. The session lifetime keeps the
  # post-transition state stable for the assertions.
  testGreetdAuth = pkgs.writeScript "test-greetd-auth.py" ''
    #!${pkgs.python3}/bin/python3
    import json, os, socket, struct, sys, time

    sock_path = os.environ.get("GREETD_SOCK")
    if not sock_path:
        print("GREETD_SOCK not set in greeter env", file=sys.stderr)
        sys.exit(1)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(sock_path)

    def send(msg):
        body = json.dumps(msg).encode()
        sock.sendall(struct.pack("=I", len(body)) + body)

    def recv():
        hdr = b""
        while len(hdr) < 4:
            chunk = sock.recv(4 - len(hdr))
            if not chunk:
                raise RuntimeError("greetd socket closed")
            hdr += chunk
        n = struct.unpack("=I", hdr)[0]
        body = b""
        while len(body) < n:
            chunk = sock.recv(n - len(body))
            if not chunk:
                raise RuntimeError("greetd socket closed mid-body")
            body += chunk
        return json.loads(body.decode())

    print("test-greeter: create_session for user=test", file=sys.stderr)
    send({"type": "create_session", "username": "test"})

    while True:
        r = recv()
        print(f"test-greeter: recv {r}", file=sys.stderr)
        kind = r.get("type")
        if kind == "auth_message":
            send({"type": "post_auth_message_response", "response": "test"})
        elif kind == "success":
            break
        elif kind == "error":
            print(f"test-greeter: auth error: {r}", file=sys.stderr)
            sys.exit(1)
        else:
            print(f"test-greeter: unexpected {r}", file=sys.stderr)
            sys.exit(1)

    print("test-greeter: start_session with sleep", file=sys.stderr)
    send({
        "type": "start_session",
        "cmd": ["${pkgs.coreutils}/bin/sleep", "infinity"],
        "env": [],
    })
    r = recv()
    print(f"test-greeter: start_session response: {r}", file=sys.stderr)
    if r.get("type") != "success":
        print("test-greeter: start_session failed", file=sys.stderr)
        sys.exit(1)

    while True:
        time.sleep(60)
  '';

  testGreeter = pkgs.writeShellScript "halmasuit-wallpaper-video-greeter" ''
    exec ${pkgs.python3}/bin/python3 ${testGreetdAuth}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-wallpaper-video";

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
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        decoder.package = halmasuit-decoder;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;

        # The whole point of this test — video wallpaper backend
        # configured against a real h264 fixture, looping.
        wallpaper = {
          type     = "video";
          source   = videoFixture;
          loop     = true;
          fallback = fallbackFixture;
        };

        # Drives the real greeter→session auth path for the
        # login-flash continuity gate. Same shape as login-flash.nix.
        greeterCommand = "${testGreeter}";
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

      # test-user.nix sets uid=1000 with group `users` (gid 100);
      # the broker's session-leader UID_MIN floor refuses gid < 1000
      # (Epic R8/R11). Give the test user a primary group at gid 1000
      # so the fork-then-drop proceeds.
      users.users.test.group = "test";
      users.groups.test.gid  = 1000;

      virtualisation = {
        memorySize = 2048; # wgpu + Mesa llvmpipe + ffmpeg decode
        cores      = 2;
        diskSize   = 2048;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Wait for the compositor's render loop to be live. scanout_active
    # fires once the first frame is composited+queued — which means
    # halmasuit has constructed its WallpaperEngine, which means
    # VideoBackend has called DecoderRelay::spawn.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=60,
    )

    # Greeter spawn is the "halmasuit reached steady state" event we
    # capture the BEFORE-pid against (matches login-flash.nix).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'greeter_spawned'",
        timeout=30,
    )

    halmasuit_pid_before = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert halmasuit_pid_before.isdigit() and int(halmasuit_pid_before) > 0, (
        f"halmasuit MainPID must be positive int, got {halmasuit_pid_before!r}"
    )
    print(f"GREETER PHASE: halmasuit pid={halmasuit_pid_before}")

    # ── Gate 1: initial decoder spawn + uid posture ──
    # arg0 is the literal string "halmasuit-decoder" (set via
    # Command::arg0 in DecoderRelay's fork-exec path); pgrep -f
    # matches the full cmdline because the 17-char name overflows
    # /proc/PID/comm's 15-char limit. The `grep -v` suffix filters
    # out the pgrep invocation itself (whose cmdline contains
    # "halmasuit-decoder").
    machine.wait_until_succeeds(
        "pgrep -af halmasuit-decoder | grep -v 'pgrep -af'",
        timeout=30,
    )
    initial_decoder_pid = machine.succeed(
        "pgrep -af halmasuit-decoder | grep -v 'pgrep -af' | "
        "awk '{print $1}' | head -1"
    ).strip()
    assert initial_decoder_pid.isdigit(), (
        f"initial decoder pid must be digit, got {initial_decoder_pid!r}"
    )

    # Epic anti-pattern: "NO running the decoder as root or with
    # elevated capabilities. Forked from halmasuit (uid 998), no
    # cap retention." VideoBackend's lazy-spawn defers the
    # DecoderRelay::spawn call to its first poll_pending — which
    # fires from the wallpaper-tick calloop timer, post-`setresuid`
    # in main.rs. The decoder inherits the configured compositor
    # uid (998 here, mapped to halmasuit-compositor) and its user-
    # namespace uid_map writes `998 998 1`, NOT `0 0 1`. This
    # assertion pins the invariant against regression.
    decoder_uid_line = machine.succeed(
        f"grep '^Uid:' /proc/{initial_decoder_pid}/status"
    ).strip()
    decoder_uid = decoder_uid_line.split()[1]
    if decoder_uid != "998":
        raise AssertionError(
            f"GATE 1 FAIL: decoder runs as uid {decoder_uid}, "
            "expected 998 (halmasuit-compositor).\n"
            "Epic anti-pattern violation: the decoder must not "
            "run as root or any elevated uid.\n"
            f"Full /proc/{initial_decoder_pid}/status Uid line: "
            f"{decoder_uid_line}"
        )
    decoder_uid_map = machine.succeed(
        f"cat /proc/{initial_decoder_pid}/uid_map"
    ).strip()
    if not decoder_uid_map.split()[0] == "998":
        raise AssertionError(
            "GATE 1 FAIL: decoder's user-namespace uid_map does "
            "not start with 998 (inner=outer=998).\n"
            f"  got: {decoder_uid_map!r}\n"
            "A uid_map starting with 0 indicates the decoder was "
            "spawned before the compositor's privilege drop "
            "(regression of task #25's lazy-spawn fix)."
        )
    # Epic Req #3: "seccomp-bpf allowlist: read, write, mmap, ...
    # Everything else → SIGSYS." /proc/<pid>/status's Seccomp line
    # reports the filter mode: 0=disabled, 1=strict, 2=filter
    # (which is what `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER)`
    # produces). 2 means our allowlist is active. Anything else is
    # a regression of task #27.
    decoder_seccomp_mode = machine.succeed(
        f"grep '^Seccomp:' /proc/{initial_decoder_pid}/status"
    ).strip()
    seccomp_value = decoder_seccomp_mode.split()[-1]
    if seccomp_value != "2":
        raise AssertionError(
            "GATE 1 FAIL: decoder seccomp mode is not "
            f"FILTER (2); got: {decoder_seccomp_mode!r}.\n"
            "Epic Req #3 violation: the seccomp-bpf allowlist "
            "must be active."
        )
    print(
        f"GATE 1 PASS: halmasuit-decoder spawned pid={initial_decoder_pid} "
        f"uid={decoder_uid} (uid_map: {decoder_uid_map}) "
        f"seccomp_mode=FILTER"
    )

    # ── Gate 4 (early — captures pre-kill PID continuity) ──
    # Wait for the broker to fork the session leader (test user
    # running `sleep infinity`); login-flash.nix uses the same
    # pgrep -f /bin/sleep idiom because the kernel's `comm` field
    # is truncated to the store path's basename portion.
    machine.wait_until_succeeds(
        "pgrep -u test -f /bin/sleep",
        timeout=60,
    )
    session_pid = machine.succeed("pgrep -u test -f /bin/sleep").strip()
    halmasuit_pid_after_session = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    print(
        f"SESSION PHASE: halmasuit pid={halmasuit_pid_after_session} "
        f"session pid={session_pid}"
    )
    if halmasuit_pid_before != halmasuit_pid_after_session:
        raise AssertionError(
            "FLASH DETECTED — halmasuit restarted across greeter→session "
            "boundary WITH a video wallpaper configured:\n"
            f"  halmasuit pid: greeter={halmasuit_pid_before} → "
            f"session={halmasuit_pid_after_session}\n"
            "\n"
            "Epic #12 must not perturb the no-flash invariant (Epic R3).\n"
        )
    print(
        f"GATE 4 PASS: halmasuit pid {halmasuit_pid_before} continuous "
        "across greeter→session with video wallpaper — no flash"
    )

    # ── Gate 5: loop-on-EOF ──
    # The 2-second fixture has ~60 frames at 30 fps. After the
    # decoder reaches EOF the lifecycle's loop_playback branch calls
    # decode::rewind, which drops the AVFormatContext + AVIO and
    # rebuilds them over the SAME wallpaper mmap with cursor at 0.
    # libavformat sees a brand-new input and starts producing
    # frames again. We gate on the "decoder: rewound for loop"
    # info-level log line, which fires every loop turnover: each
    # rewind proves a full file's worth of frames was produced
    # AND the EOF-loop path actually fired (a stronger invariant
    # than counting individual frames). The per-frame send log is
    # debug-level and intentionally suppressed at production
    # RUST_LOG=info — counting it from journald is fragile against
    # log-level tuning.
    #
    # Pre-T24 the loop livelocked at "open returned EOF" forever,
    # producing exactly one decode pass before going silent (zero
    # rewinds).
    machine.wait_until_succeeds(
        # >= 2 rewinds = 60-frame fixture played through at least
        # 3 times. Conservative margin against PTS-pacing variance
        # so a slow CI runner still satisfies the gate.
        "test \"$(journalctl -u halmasuit | "
        "grep -c 'decoder: rewound for loop')\" -ge 2",
        timeout=30,
    )
    rewind_count = machine.succeed(
        "journalctl -u halmasuit | "
        "grep -c 'decoder: rewound for loop'"
    ).strip()
    print(
        f"GATE 5 PASS: decoder completed {rewind_count}+ loop "
        "iterations (loop-on-EOF working; mmap-AVIO rewind path "
        "intact)"
    )

    # ── Gate 2: crash recovery within budget ──
    # SIGKILL the current decoder. The compositor's wallpaper-tick
    # calloop timer (registered in main.rs at 100 ms cadence) calls
    # tick_wallpaper, which delegates through DrmBackend →
    # WallpaperEngine → VideoBackend::poll_pending → relay.poll_frames.
    # poll_frames sees IPC EOF from the dead decoder, calls
    # respawn() under budget, swapping in a new chan/pidfd. We wait
    # until a NEW pid (distinct from initial) appears.
    machine.succeed(f"kill -9 {initial_decoder_pid}")
    machine.wait_until_succeeds(
        f"! kill -0 {initial_decoder_pid} 2>/dev/null",
        timeout=10,
    )
    machine.wait_until_succeeds(
        "pgrep -af halmasuit-decoder | grep -v 'pgrep -af'",
        timeout=30,
    )
    respawn_pid = machine.succeed(
        "pgrep -af halmasuit-decoder | grep -v 'pgrep -af' | awk '{print $1}' | head -1"
    ).strip()
    if respawn_pid == initial_decoder_pid:
        raise AssertionError(
            f"GATE 2 FAIL: pgrep returned the killed pid {initial_decoder_pid} "
            "after kill -9 — relay did not respawn"
        )
    print(f"GATE 2 PASS: relay respawned decoder pid={respawn_pid} "
          f"(was {initial_decoder_pid})")

    # ── Gate 3: budget exhaustion ──
    # MAX_RESTARTS_PER_WINDOW = 3 in 10s. Gate 2 has already used
    # 1 failure. We need at least 3 MORE failures inside the 10s
    # rolling window. Per-kill setup in nixos-test (`succeed` + the
    # default 1s poll cadence of wait_until_succeeds) is slow enough
    # that four kills spread across the gate's iterations can
    # span >10s — older entries get pruned by note_failure's window
    # math, and count_after stays ≤3, so the relay respawns instead
    # of going dead. Solution: do the rapid kills in ONE bash loop
    # on the VM side, no test-driver round-trips between iterations.
    # Drive 5 rapid kills in a single VM-side bash loop. The 0.3s
    # sleep between iterations is NOT the test-driver `time.sleep`
    # the state-based-polling memory warns against — it's a tight
    # pacing INSIDE one bash command, well above the relay's 100 ms
    # tick. Doing one Python iteration per kill is unworkable here:
    # nixos-test-driver's SSH/QEMU-monitor round-trip is ~1–2s per
    # `machine.succeed`, which blows the 5 kills past the relay's
    # 10s restart-budget window, so older failures get pruned and
    # the count never exceeds the threshold. Five kills (one extra
    # past the 4 needed to exhaust) is robust against pgrep racing
    # with the respawn cycle. `|| true` keeps a missing pgrep from
    # tripping `set -e`.
    machine.succeed(
        "for i in 1 2 3 4 5; do "
        "  pid=$(pgrep -af halmasuit-decoder | "
        "        grep -v 'pgrep -af' | awk '{print $1}' | head -1 "
        "        || true); "
        "  if [ -n \"$pid\" ]; then kill -9 \"$pid\" 2>/dev/null || true; fi; "
        "  sleep 0.3; "
        "done"
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'decoder restart budget exhausted'",
        timeout=30,
    )
    # After dead, no respawn happens — pgrep stays empty.
    machine.wait_until_succeeds(
        "! pgrep -af halmasuit-decoder | grep -qv 'pgrep -af'",
        timeout=10,
    )
    print("GATE 3 PASS: budget exhausted, relay dead, no respawn")

    # ── Gate 3 continuation: halmasuit MUST survive ──
    # The whole reason halmasuit-decoder is a sandboxed subprocess is
    # that a buggy/exploitable video file cannot kill the compositor.
    # After 4 forced decoder crashes, halmasuit.service stays
    # ActiveState=active AND its MainPID is identical to what we
    # captured before any kills.
    halmasuit_status = machine.succeed(
        "systemctl show -p ActiveState --value halmasuit.service"
    ).strip()
    if halmasuit_status != "active":
        raise AssertionError(
            f"GATE 3 FAIL: halmasuit died after decoder budget exhaustion: "
            f"ActiveState={halmasuit_status}\n"
            "A crashing decoder MUST NOT take down the compositor — "
            "that is the entire reason for the sandbox subsystem."
        )
    halmasuit_pid_final = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    if halmasuit_pid_final != halmasuit_pid_before:
        raise AssertionError(
            f"GATE 3 FAIL: halmasuit restarted after decoder kills:\n"
            f"  before={halmasuit_pid_before} final={halmasuit_pid_final}\n"
            "halmasuit must be PID-stable across decoder failures."
        )
    print(
        f"GATE 3 PASS (continuation): halmasuit pid={halmasuit_pid_final} "
        "still active, identical to before decoder kills"
    )

    # ── Gate 6: fallback-to-ImageBackend on relay-dead ──
    # Epic Req #4 ("After exhausted, drop the relay and fall back to
    # ImageBackend") and Req #10 ("Asserts fallback: after N forced
    # crashes, halmasuit falls back to image without crashing"). The
    # operator configured `wallpaper.fallback = fallbackFixture` in
    # the test machine config; after Gate 3's budget exhaustion the
    # WallpaperEngine should swap the active backend from
    # VideoBackend to ImageBackend on the next render tick.
    #
    # Headless VM tests can't pixel-distinguish the fallback (per
    # CLAUDE.md "Test-VM rendering gotcha"); instead assert via the
    # canonical log line emitted by
    # `WallpaperEngine::maybe_swap_fallback`.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'wallpaper: swapping to fallback image (relay-dead)'",
        timeout=15,
    )
    print(
        "GATE 6 PASS: WallpaperEngine swapped to ImageBackend "
        "fallback after relay-dead"
    )

    print("visual-wallpaper-video: ALL GATES PASSED")
  '';
}
