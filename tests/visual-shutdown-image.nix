# tests/visual-shutdown-image.nix — Epic #61 R3.4: image cell of the
# wallpaper-shutdown-survival matrix.
#
# Pairs with visual-shutdown-pivot-survival.nix (shader cell, asserts
# phash-progression + frame-counter advancing across the shutdown
# window). Image wallpapers are STATIC by nature: the wallpaper-engine
# tick is NOT registered for them (no animation work to drive), so
# the kernel just keeps scanning out the last-flipped framebuffer.
# The shutdown survival invariants asserted here are accordingly
# narrower: process stays alive, same PID throughout, no coredump.
# Frame-counter advancement and phash-progression deliberately do
# NOT apply.
#
# Asserted invariants (all hard gates):
#   1. `Successfully changed into root pivot` marker appears on
#      the serial console — proves systemd-shutdown completed.
#   2. At least one `halmasuit-shutdown-liveness pid=N` line
#      appears AFTER the post-pivot marker — proves halmasuit's
#      same PID survived the pivot.
#   3. EVERY liveness line in the trace carries the same PID, and
#      that PID matches the pre-shutdown halmasuit MainPID — PID
#      continuity, no respawn.
#   4. No `coredump:` kernel log for halmasuit's PID — the #60
#      double-panic-abort regression hasn't returned.
#
# The third matrix cell (video) lands in a follow-up subtask
# (R3.5) and uses the same shape as the shader cell.

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

  niriConfig = pkgs.writeText "niri-shutdown-image-config.kdl" ''
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

  sessionCmd = pkgs.writeShellScript "halmasuit-shutdown-image-session" ''
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
  name = "visual-shutdown-image";

  skipTypeCheck = true;

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, lib, ... }:
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
        # Image wallpaper — the static cell of the matrix. The
        # wallpaper-engine tick is not registered (image needs no
        # animation work); shutdown survival assertions here are
        # narrower than the shader/video cells: no frame-counter
        # advancement, no phash progression.
        wallpaper = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand = "${pkgs.writeShellScript "halmasuit-shutdown-image-greeter" ''
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

      # With useBootLoader = true (see virtualisation block below), the
      # disk image only contains the closure of nodes.machine's system.
      # The 9p host-share that normally exposes the full host /nix/store
      # is gone, so anything referenced by store path from the test
      # driver (sessionCmd, niri) but NOT in a normal NixOS closure
      # path has to be pulled in explicitly. system.extraDependencies
      # adds them to the closure without putting them in PATH.
      system.extraDependencies = [ sessionCmd niri ];

      systemd.tmpfiles.rules = [
        "d /run/halmasuit-niri 0700 alice alice -"
      ];
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

      # #60 fix: boot from a real disk image (ext4 on virtio-blk),
      # with the entire system closure installed on /. This matches
      # production NixOS layout where /nix/store is a directory on
      # the root filesystem — NOT a separate mount. systemd-shutdown
      # never unmounts /; it only remounts it RO, which the kernel
      # permits even on mmap-busy filesystems. Halmasuit's code-page
      # mappings stay live through the entire shutdown sequence.
      #
      # systemd-boot is REQUIRED here (not the host's GRUB default):
      # the disk image install runs `switch-to-configuration boot`
      # inside `nixos-enter`'s chroot, which has no access to the
      # host firmware's EFI NVRAM. GRUB-EFI's installation step uses
      # `efibootmgr` to write Boot#### entries to NVRAM; in a chroot
      # that fails silently, leaving the OVMF firmware with nothing
      # to boot ("BdsDxe: No bootable option or device was found").
      # systemd-boot bypasses NVRAM entirely — it writes a fallback
      # /EFI/BOOT/BOOTX64.EFI which OVMF finds via its built-in
      # removable-media boot path. The `canTouchEfiVariables = false`
      # flag tells the install hook to NOT try `efibootmgr` either.
      boot.loader.systemd-boot.enable    = true;
      boot.loader.efi.canTouchEfiVariables = false;
      boot.loader.grub.enable            = lib.mkForce false;

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];

        useBootLoader     = true;
        useEFIBoot        = true;
        mountHostNixStore = false;
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
    # The pivot marker is `Successfully changed into root pivot`,
    # logged by systemd-shutdown the instant after it execve's into
    # the shutdown initramfs — that is THE post-pivot moment, and any
    # halmasuit liveness line after it proves the same PID survived
    # the rootfs→shutdownRamfs transition. (Earlier versions of this
    # test gated on `Reached target System Power Off`, which fires
    # while systemd is still in the rootfs; the present marker is the
    # actual cutover.)
    pivot_re = re.compile(
        r"Successfully changed into root pivot", re.MULTILINE
    )
    pivot_match = pivot_re.search(console)
    if pivot_match is None:
        tail = "\n".join(console.splitlines()[-100:])
        raise AssertionError(
            "Could not locate `Successfully changed into root pivot` "
            "marker in serial console. systemd-shutdown either did "
            "not complete the pivot or the log line did not make it "
            "to the console.\n\nLast 100 console lines:\n" + tail
        )
    pivot_offset = pivot_match.start()
    pivot_line = console.count("\n", 0, pivot_offset)
    print(f"PASS: located post-pivot marker at console line {pivot_line}")

    # ── Heartbeat-after-pivot assertion ────────────────────────────
    # halmasuit's always-on liveness timer writes
    # `halmasuit-shutdown-liveness pid=N frames=M` every
    # HALMASUIT_LIVENESS_INTERVAL_MS to stdout. Production has
    # `StandardOutput=file:/dev/kmsg`, which makes systemd open
    # /dev/kmsg directly and pass the fd to halmasuit — bypassing
    # the journald-stdout pipe, so the bytes still reach the kernel
    # ring buffer after systemd-journald is killed mid-shutdown and
    # across the rootfs→shutdownRamfs pivot.
    hb_re = re.compile(
        r"halmasuit-shutdown-liveness pid=(\d+) frames=(\d+)"
    )

    post_pivot = console[pivot_offset:]
    post_pivot_hbs = list(hb_re.finditer(post_pivot))
    if not post_pivot_hbs:
        post_pivot_window = "\n".join(post_pivot.splitlines()[:80])
        raise AssertionError(
            "Production halmasuit did NOT survive the rootfs→"
            "shutdownRamfs pivot: 0 `halmasuit-shutdown-liveness` "
            "kmsg lines after the `Successfully changed into root "
            "pivot` marker. Either SurviveFinalKillSignal=yes "
            "regressed, or the binary isn't in shutdownRamfs's "
            "storePaths, or graceful_shutdown started exiting the "
            "process instead of letting the loop continue."
            "\n\nFirst 80 lines after the marker:\n"
            + post_pivot_window
        )
    last_post = post_pivot_hbs[-1]
    print(
        f"PASS: {len(post_pivot_hbs)} halmasuit-shutdown-liveness line(s) "
        f"emitted AFTER the post-pivot marker; last pid={last_post.group(1)}, "
        f"last frames={last_post.group(2)}"
    )

    # NOTE: image wallpapers don't register the wallpaper-engine tick
    # (image is static; the kernel scans out the last framebuffer
    # without re-rendering), so the shader/video cells' frame-counter
    # advancement assertion and phash-progression assertion do NOT
    # apply here. The matrix shape is intentional: each cell asserts
    # the invariants that are MEANINGFUL for its wallpaper variant.

    # ── No-coredump assertion ──────────────────────────────────────
    coredump_re = re.compile(rf"coredump:\s+{halmasuit_pid}\(halmasuit\)")
    cd = coredump_re.search(console)
    if cd:
        raise AssertionError(
            f"halmasuit PID {halmasuit_pid} took a coredump-class signal "
            f"during shutdown — the regression #60 fixed has returned. "
            f"Match: {console[max(0, cd.start()-80):cd.end()+80]}"
        )
    print("PASS: no coredump for halmasuit MainPID throughout shutdown")

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

    print("visual-shutdown-image: ALL ASSERTIONS PASSED")
  '';
}
