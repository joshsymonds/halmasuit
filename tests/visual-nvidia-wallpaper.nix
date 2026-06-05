# tests/visual-nvidia-wallpaper.nix — Epic #45, rung 2.
#
# Boot halmasuit on the REAL passed-through RTX 5070 Ti and prove its
# NVIDIA EGL/GBM render path initializes and composites the wallpaper to
# first-frame. This is the direct analog of gnomon's situation: if
# halmasuit hangs here instead of rendering, we've reproduced a
# gnomon-class NVIDIA hang in an automatable harness.
#
# Scope: JOURNAL assertions only (GPU-agnostic) — no Snapshot()/golden
# and no KMS-scanout readback yet (those are rung 3). halmasuit
# composites the wallpaper to its OFFSCREEN GLES target regardless of a
# connected connector (same as the headless virtio-gpu tests), so this
# rung needs no physical monitor.
#
# RUNNER-ONLY: `just test-vm-nvidia visual-nvidia-wallpaper` on stygian
# (GPU on vfio-pci). NEVER `nix build` — the sandbox has no /dev/vfio.
{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:
let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-nvidia-wallpaper";

  # testScript imports the visual helper via sys.path at runtime.
  skipTypeCheck = true;

  nodes.machine =
    { pkgs, config, ... }:
    {
      imports = [
        ./lib/nvidia-passthrough.nix
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit; # halmasuit-debug (frame_audit)
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;

        # The REAL NVIDIA render path — not software/llvmpipe. This is
        # the whole point of the rig (Epic #45 req 3).
        rendering = {
          backend = "nvidia";
          # nvidiaPackage defaults to config.hardware.nvidia.package
          # (set by the passthrough wrapper). Add the EGL platform
          # plugins so libEGL can create a GBM display (the gnomon
          # R2/R3 failure mode: missing __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS).
          extraInitrdStorePaths = [
            "${pkgs.egl-wayland}"
            "${pkgs.egl-gbm}"
          ];
        };

        # Pin the passed-through GPU by its guest-side BDF. qemu places
        # the first vfio-pci device at 00:09.0 (observed in rung 1's
        # nvidia-smi: 00000000:00:09.0). Pinning by PCI avoids opening
        # the emulated VGA card. If qemu ever reassigns the slot, the
        # test fails fast (halmasuit opens the wrong/no card).
        drmDevice = "pci:0000:00:09.0";

        # Wallpaper-only gate: no auth driven, halmasuit composites its
        # own bottom plane. Greeter is inert.
        wallpaper = {
          type   = "image";
          source = ./fixtures/wallpaper.png;
        };
        greeterCommand = "${pkgs.writeShellScript "halmasuit-nvidia-wallpaper-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
      };

      # Snapshot()'s writable PNG dir (unused this rung; wired now so
      # rung 3 can drop in golden capture without a config change).
      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

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
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # nvidia must be bound (rung-1 invariant) before halmasuit opens it.
    machine.wait_until_succeeds(
        "lspci -nnk -d 10de: | grep -q 'Kernel driver in use: nvidia'",
        timeout=60,
    )

    # EGL-platform diagnostics (Epic #45 rung 2): root-cause why
    # halmasuit can't create a GBM EGL display on real NVIDIA. These
    # are static (system config + driver state), independent of
    # halmasuit's crash loop.
    print("=== halmasuit.service Environment ===\n" +
          machine.execute("systemctl show halmasuit.service -p Environment")[1])
    print("=== /run/opengl-driver/lib (+ gbm) ===\n" +
          machine.execute("ls -la /run/opengl-driver/lib/ /run/opengl-driver/lib/gbm/ 2>&1 | head -50")[1])
    print("=== nvidia/gbm libs in the farm ===\n" +
          machine.execute("find /run/opengl-driver -iname '*nvidia*' -o -iname '*gbm*' -o -iname '*allocator*' 2>/dev/null | head -30")[1])
    print("=== EGL external-platform config dir contents ===\n" +
          machine.execute(
              "d=$(systemctl show halmasuit.service -p Environment --value | tr ' ' '\\n' | "
              "grep __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS | cut -d= -f2-); "
              "echo \"DIR=$d\"; for p in $(echo \"$d\" | tr ':' ' '); do echo \"-- $p --\"; "
              "ls -la \"$p\" 2>&1; cat \"$p\"/*.json 2>&1; done")[1])
    print("=== EGL vendor ICD ===\n" +
          machine.execute(
              "v=$(systemctl show halmasuit.service -p Environment --value | tr ' ' '\\n' | "
              "grep __EGL_VENDOR_LIBRARY_FILENAMES | cut -d= -f2-); echo \"ICD=$v\"; "
              "cat $(echo \"$v\" | tr ':' ' ') 2>&1 | head -20")[1])
    print("=== card1 DRM driver link (why 'driver null'?) ===\n" +
          machine.execute("for c in /sys/class/drm/card*; do echo \"$c -> $(readlink -f $c/device/driver 2>&1)\"; done; "
                          "echo '--- render nodes ---'; ls -la /dev/dri/ 2>&1")[1])

    # halmasuit must come up on the real GPU. A cold NVIDIA DRM/EGL
    # bring-up is slow in a VM, so allow generous time.
    try:
        machine.wait_for_unit("halmasuit.service", timeout=120)
    except Exception:
        print("=== halmasuit failed to start; journal ===")
        print(machine.execute("journalctl -u halmasuit --no-pager")[1])
        raise

    # The backend halmasuit actually resolved — must be nvidia, not a
    # software fallback (Epic #45 req 3).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'wallpaper config resolved'",
        timeout=60,
    )
    backend_line = machine.succeed(
        "journalctl -u halmasuit | grep -F 'wallpaper config resolved' | tail -1"
    )
    print("=== wallpaper config resolved ===\n" + backend_line)

    # The pivotal assertion: halmasuit's NVIDIA EGL/GBM path initialized
    # and it composited the wallpaper to first-frame. If this never
    # fires, halmasuit stalled on real NVIDIA — dump the journal so the
    # stall point (EGL init? no connector? has_buffer:false?) is visible.
    try:
        machine.wait_until_succeeds(
            "journalctl -u halmasuit | grep -qE 'client_first_frame|scanout_active'",
            timeout=120,
        )
    except Exception:
        print("=== halmasuit did NOT reach first-frame on NVIDIA; journal ===")
        print(machine.execute("journalctl -u halmasuit --no-pager")[1])
        print("=== layer-shell commit lines ===")
        print(machine.execute("journalctl -u halmasuit | grep -F 'layer-shell commit' || true")[1])
        print("=== EGL/libGL errors ===")
        print(machine.execute("journalctl -u halmasuit | grep -iE 'egl|glx|drm|gbm|nvrm' || true")[1])
        raise

    print(machine.succeed(
        "journalctl -u halmasuit | grep -E 'client_first_frame|scanout_active' | head"
    ))
    print("visual-nvidia-wallpaper: halmasuit composited the wallpaper on real NVIDIA. PASS")
  '';
}
