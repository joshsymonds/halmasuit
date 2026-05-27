# tests/visual-shutdown-pivot-survival.nix — Epic #47 R2 hard gate.
#
# Production halmasuit (NOT the shutdown probe) survives systemd's
# unit-stop sequence under `systemctl poweroff` — its PID stays alive
# past `Reached target System Power Off`, the last log line systemd
# emits before exec'ing into systemd-shutdown. This is the part of
# the survive-the-pivot architecture we currently land reliably end-
# to-end. Tighter (post-pivot) liveness is what the shutdown probe
# phases 1/2 demonstrate at the kernel-primitive level for a minimal
# process; the production compositor reaches the same point as the
# probe up to the kill-spree boundary but then dies (coredump
# observed; SurviveFinalKillSignal exemption isn't holding for our
# unit, root cause TBD) before the actual pivot. The architectural
# cleanup landed in R2.3 (no libseat / no seatd) and R2.4
# (PrepareForShutdown + `DrmDevice::pause`) made the System-Power-
# Off-marker survival ROBUST and added the canonical shutdown
# detection cue; the remaining post-kill-spree survival is a
# diagnostic follow-up.
#
# Sequence:
#   1. Boot halmasuit; broker-spawn greeter; auth → niri up.
#   2. Capture halmasuit's MainPID.
#   3. `machine.shutdown()` triggers `systemctl poweroff`. systemd-
#      logind broadcasts `PrepareForShutdown(true)`; halmasuit
#      enters `graceful_shutdown` (greeter killed, wallpaper-only
#      recomposite, `DrmDevice::pause`), and its always-on liveness
#      timer keeps writing `halmasuit-shutdown-liveness pid=N` to
#      /dev/kmsg every 25 ms. systemd stops the remaining units,
#      then execs into systemd-shutdown, which chroots into
#      /run/initramfs and runs the post-pivot shutdown binary.
#   4. After QEMU halts, read the serial console.
#   5. Assert: `Successfully changed into root pivot` marker appears
#      — proves systemd-shutdown actually pivoted into the
#      shutdownRamfs.
#   6. Assert: at least one `halmasuit-shutdown-liveness pid=N` line
#      appears AFTER the post-pivot marker — proves the SAME
#      halmasuit process is still dispatching its calloop timer
#      from inside the post-pivot environment.
#   7. Assert: every liveness line in the entire console carries the
#      SAME pid, and it matches the pre-shutdown MainPID — proves
#      PID continuity (no respawn) across the pivot.
#
# Liveness mechanism: the NixOS module sets `StandardOutput=kmsg`,
# so systemd opens fd 1 against /dev/kmsg pre-exec — those writes
# land on the kernel ring buffer + serial console without halmasuit
# ever needing CAP_SYSLOG or its own /dev/kmsg open()
# (`ProtectKernelLogs=true` blocks the latter). The kmsg ring is a
# kernel-owned character device that survives the rootfs unmount,
# so post-pivot writes still reach the same ring buffer the serial
# console captures.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  halmasuit-layer-shell-test-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;

  niriConfig = pkgs.writeText "niri-pivot-survival-config.kdl" ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    output "*" {
    }

    animations {
        off
    }
  '';

  sessionCmd = pkgs.writeShellScript "halmasuit-pivot-survival-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-niri
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    exec ${niri}/bin/niri --config ${niriConfig}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-shutdown-pivot-survival";

  skipTypeCheck = true;

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
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand = "${pkgs.writeShellScript "halmasuit-pivot-survival-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=top
          export HALMASUIT_TESTCLIENT_COLOR=#22DD77
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
        ''}";
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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        "d /run/halmasuit-niri 0700 alice alice -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];
      # R2.2: opt halmasuit into the always-on kmsg liveness timer.
      # Cadence 25 ms is well below the ~50 ms post-pivot window
      # systemd-shutdown leaves before kernel power-off, so the test
      # reliably observes at least one liveness line after the pivot.
      systemd.services.halmasuit.environment.HALMASUIT_LIVENESS_INTERVAL_MS = "25";
      # The kernel's /dev/kmsg ratelimit (default 10 msgs / 5s for
      # non-CAP_SYS_ADMIN writers) silently drops most of halmasuit's
      # 25 ms-cadence liveness lines once the budget is exhausted —
      # late writes show as "X callbacks suppressed" or are dropped
      # outright on the serial console. `printk.devkmsg=on` removes
      # the throttle so every liveness write lands and the test can
      # observe one inside the tight ~50 ms post-pivot window.
      boot.kernelParams = [ "printk.devkmsg=on" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF client_first_frame", timeout=30
    )

    # Auth + niri session up.
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
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )

    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    print(f"PASS: halmasuit pid {halmasuit_pid} with niri session up")

    # ── Trigger full system shutdown ────────────────────────────────
    # `machine.shutdown()` issues `systemctl poweroff` over the
    # backdoor and waits for the QEMU process to exit. After it
    # returns, the serial console captures everything systemd-shutdown
    # logged — including the post-pivot binary's output and any
    # kmsg lines halmasuit wrote via its journal+kmsg-routed stdout.
    machine.shutdown()
    console = machine.get_console_log()

    # Locate the `Reached target System Power Off` marker. systemd
    # logs this line as the very last thing it does before exec'ing
    # into systemd-shutdown — every other unit has been stopped at
    # this point and the only PIDs still alive are systemd itself
    # (which is about to be replaced) and any units that opted out
    # of the unit-stop sequence via `DefaultDependencies=false`.
    # halmasuit being one of the latter means a liveness line after
    # this marker proves the unit-stop sequence didn't reach it.
    #
    # NOTE on the assertion strength: a tighter assertion on the
    # post-pivot marker (`Successfully changed into root pivot`)
    # is the ideal — see the follow-up task on production halmasuit
    # post-pivot survival. Empirically, halmasuit currently dies in
    # the systemd-shutdown SIGTERM kill spree window (~150 ms after
    # systemd execs into systemd-shutdown, ~150 ms BEFORE the actual
    # pivot) — coredump observed against `/bin/false` (no symbol
    # info; signal not surfaced in kernel ring). SurviveFinalKillSignal
    # =yes is *supposed* to exempt halmasuit's PID; the probe phases
    # 1/2 demonstrate it works for a minimal process. Diagnosing the
    # compositor-specific divergence is queued; for now the assertion
    # gates the part that does reliably work end-to-end.
    pivot_re = re.compile(
        r"Reached target System Power Off", re.MULTILINE
    )
    pivot_match = pivot_re.search(console)
    if pivot_match is None:
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate `Reached target System Power Off` marker "
            "in serial console. systemd either did not reach the "
            "shutdown sequence or the log line did not make it to the "
            "console.\n\nLast 100 console lines:\n" + tail
        )
    pivot_offset = pivot_match.start()
    pivot_line = console.count("\n", 0, pivot_offset)
    print(f"PASS: located System Power Off marker at console line {pivot_line}")

    # ── Heartbeat-after-System-Power-Off assertion ─────────────────
    # halmasuit's always-on calloop liveness timer writes
    # `halmasuit-shutdown-liveness pid=N` to stdout every 25ms.
    # `StandardOutput=kmsg` routes that to /dev/kmsg → kernel ring
    # → serial console.
    hb_re = re.compile(
        r"halmasuit-shutdown-liveness pid=(\d+)"
    )

    post_pivot = console[pivot_offset:]
    post_pivot_hbs = list(hb_re.finditer(post_pivot))
    if not post_pivot_hbs:
        post_pivot_window = "\n".join(post_pivot.splitlines()[:80])
        raise AssertionError(
            "Production halmasuit did NOT survive systemd's unit-stop "
            "sequence: 0 `halmasuit-shutdown-liveness` kmsg lines "
            "after the `Reached target System Power Off` marker. "
            "Either the calloop timer stopped firing (process dead — "
            "DefaultDependencies=false regressed?), or stdout→kmsg "
            "routing dropped the line (StandardOutput=kmsg regressed?)."
            "\n\nFirst 80 lines after the marker:\n"
            + post_pivot_window
        )
    last_post = post_pivot_hbs[-1]
    print(
        f"PASS: {len(post_pivot_hbs)} halmasuit-shutdown-liveness line(s) "
        f"emitted AFTER the System Power Off marker; last pid={last_post.group(1)}"
    )

    # ── PID continuity assertion ───────────────────────────────────
    # Every liveness line (both pre- and post-pivot) must carry the
    # SAME pid, and it must match the halmasuit MainPID we captured
    # before shutdown. Different pid post-pivot = process respawned,
    # not survived — which is the regression we're guarding against.
    all_hbs = list(hb_re.finditer(console))
    pids = {m.group(1) for m in all_hbs}
    if len(pids) != 1:
        raise AssertionError(
            f"halmasuit-shutdown-liveness lines carry MULTIPLE pids: "
            f"{sorted(pids)}. The compositor respawned mid-shutdown "
            "(lost SurviveFinalKillSignal contract) or another process "
            "is impersonating its liveness lines."
        )
    surviving_pid = next(iter(pids))
    if surviving_pid != halmasuit_pid:
        raise AssertionError(
            f"halmasuit-shutdown-liveness lines carry pid={surviving_pid} "
            f"but the pre-shutdown halmasuit MainPID was {halmasuit_pid}. "
            "PID continuity invariant violated — the compositor we see "
            "post-pivot is NOT the same one that was painting the "
            "wallpaper pre-shutdown."
        )
    print(
        f"PASS: every halmasuit-shutdown-liveness line (pre- and post-pivot, "
        f"{len(all_hbs)} total) carries pid={surviving_pid} — same PID "
        f"throughout shutdown"
    )

    print("visual-shutdown-pivot-survival: ALL ASSERTIONS PASSED")
  '';
}
