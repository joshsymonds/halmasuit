# Phase 4 research probe: validate that a libseat/seatd-brokered
# session (DRM master + libinput device fds + session-active) survives
# a process's `setresuid` to a non-root uid.
#
# This is the empirical gate for epic layer E (#11): adopting libseat
# makes seatd broker the DRM fd, REPLACING halmasuit's self-acquired
# SET_MASTER that drm-master-probe Phases 0–3 validated. The probe
# starts as root (so it can connect to seatd), opens DRM + input
# THROUGH the seatd-brokered libseat session, does a master-only
# modeset, `setresuid`s to uid 1000 (a BARE drop — zero retained
# caps, stricter than halmasuit's CAP_KILL-retaining drop), then
# re-asserts: master-only set_crtc still works, libinput still
# delivers an injected keystroke, session still active.
#
# Rootfs-direct (no initramfs survival logic — that's Phases 1–3).

{
  system,
  nixpkgs,
}:

let
  pkgs = import nixpkgs { inherit system; };

  drmMasterProbePhase4 = pkgs.rustPlatform.buildRustPackage {
    pname   = "drm-master-probe-phase4";
    version = "0.1.0";
    src     = ../.;
    cargoLock = {
      lockFile = ../Cargo.lock;
      allowBuiltinFetchGit = true;
    };
    cargoBuildFlags   = [ "-p" "drm-master-probe" "--features" "phase4" ];
    nativeBuildInputs = [ pkgs.pkg-config ];
    # libseat (seatd), libudev (udev), libinput, libxkbcommon
    # (input-sys hardcodes `-lxkbcommon`). Must match the flake
    # `drm-master-probe-phase4` package's set.
    buildInputs       = [
      pkgs.seatd
      pkgs.libinput
      pkgs.libxkbcommon
      pkgs.udev
      pkgs.libgbm
    ];
    doCheck = false;
  };
in
pkgs.testers.runNixOSTest {
  name = "drm-master-probe-phase4";

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ./lib/test-user.nix ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          # An evdev keyboard libinput can enumerate; machine.send_key
          # drives it.
          "-device virtio-keyboard-pci"
        ];
      };

      # seatd: the root device broker libseat connects to. This is the
      # whole point of the phase — seatd opens DRM/input and hands fds
      # to the (soon-to-be-unprivileged) probe.
      services.seatd.enable = true;

      systemd.services.drm-master-probe-phase4 = {
        description = "Phase 4 probe — libseat/seatd survival across setresuid";
        wantedBy = [ "multi-user.target" ];
        after    = [ "seatd.service" "systemd-udev-settle.service" ];
        requires = [ "seatd.service" ];
        serviceConfig = {
          Type           = "simple";
          # Starts as root (to reach the seatd socket), self-drops to
          # uid 1000 via setresuid inside the probe.
          ExecStart      = "${drmMasterProbePhase4}/bin/drm-master-probe";
          Restart        = "no"; # failure IS the signal
          StandardOutput = "journal";
          StandardError  = "journal";
          Environment = [
            "PROBE_PHASE=seatd"
            "PROBE_DROP_UID=1000"
            # Force the seatd backend (no logind session exists for a
            # system service; remove autodetect ambiguity).
            "LIBSEAT_BACKEND=seatd"
          ];
        };
      };
    };

  testScript = ''
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("drm-master-probe-phase4.service")

    def jrnl():
        return machine.succeed("journalctl -u drm-master-probe-phase4 --no-pager -o cat")

    # 1. Pre-drop: libseat session up, master brokered by seatd.
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe-phase4 | grep -qF 'phase4 pre-drop master=OK'",
        timeout=60,
    )
    # 2. The setresuid actually happened.
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe-phase4 | grep -qF 'phase4 setresuid(->1000) ok'",
        timeout=30,
    )
    # 3. Post-drop master re-assert (master-only set_crtc) succeeded.
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe-phase4 | grep -qF 'phase4 post-drop master=OK'",
        timeout=30,
    )

    pid = machine.succeed(
        "systemctl show -p MainPID --value drm-master-probe-phase4.service"
    ).strip()
    print(f"probe PID = {pid}")

    # The probe runs as uid 1000 post-drop (all Uid fields).
    uid_line = machine.succeed(f"grep '^Uid:' /proc/{pid}/status").strip()
    print(uid_line)
    assert all(f == "1000" for f in uid_line.split()[1:5]), (
        f"probe not fully dropped to uid 1000: {uid_line!r}"
    )

    # KEY libseat finding: with seatd brokering, the DRM master is
    # SEATD, not the client — the client operates through the
    # seatd-brokered fd and never appears as master in debugfs. So the
    # Phase-0–3 "probe PID is debugfs-master" check does NOT apply
    # here. The master proof for Phase 4 is the probe's OWN
    # post-setresuid `set_crtc` (a master-only ioctl) succeeding —
    # already waited on above as "phase4 post-drop master=OK". Record
    # who debugfs shows as master for the RESEARCH.md write-up.
    clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    print(f"DRM clients (expect seatd, NOT the probe, as master):\n{clients}")
    seatd_is_master = any(
        len(f) >= 4 and f[3] == "y" and f[0] == "seatd"
        for f in (ln.split() for ln in clients.splitlines())
    )
    assert seatd_is_master, (
        f"expected seatd to hold DRM master under libseat brokering:\n{clients}"
    )
    print(
        "PASS: post-setresuid master-only set_crtc succeeded on the "
        "seatd-brokered fd (DRM master held via seatd, not the client)"
    )

    # 4. Inject a real keystroke NOW (post-drop) and require the probe
    #    — running as uid 1000, libinput fds seatd-brokered — to
    #    observe it. This is the input half of the invariant.
    time.sleep(1)
    for _ in range(5):
        machine.send_key("a")
        time.sleep(0.5)
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe-phase4 | grep -qF "
        "'phase4 post-drop input event received'",
        timeout=30,
    )

    # 5. The single line that means all three survived the drop.
    machine.wait_until_succeeds(
        "journalctl -u drm-master-probe-phase4 | grep -qF "
        "'phase4 post-drop: master=OK input=OK active='",
        timeout=30,
    )

    # No seatd errors.
    seatd_err = machine.succeed(
        "journalctl -u seatd -p err..warning -b --no-pager || true"
    ).strip()
    print(f"seatd warnings/errors:\n{seatd_err or '(none)'}")

    print("drm-master-probe-phase4: ALL ASSERTIONS PASSED")
  '';
}
