# tests/visual-wallpaper-render-rate.nix — Epic #53.
#
# The RED gate for the vblank-driven-render fix. halmasuit drives
# animated wallpaper backends (shader/video) from a fixed-interval
# calloop timer (main.rs ~5476: `ToDuration(from_millis(100))` = 10 fps)
# instead of the display vblank. That caps + jitters animation ("refresh
# seems low" + time-based brightness lurch = "flashes darker/lighter").
#
# This is GPU-AGNOSTIC (the throttle is in the render loop), so it is
# tested HERMETICALLY here — no GPU, no stygian. The NVIDIA scanout path
# is covered separately by visual-nvidia-shader.nix.
#
# Mechanism: with HALMASUIT_LIVENESS_INTERVAL_MS set, halmasuit emits
# `halmasuit-shutdown-liveness pid=N frames=M` to kmsg, where `frames` is
# the always-on render counter (DrmBackend::frame_counter, bumped once
# per render that queued a frame, drm.rs:1682). Sampling `frames` over a
# wall-clock window gives the real render rate. An ANIMATED shader (output
# changes every frame → damage every frame) means the render rate is the
# animation rate.
#
# RED: ~10 fps (the 100 ms timer). GREEN (after vblank-driven render):
# ~display refresh. Asserts >= 50 fps.
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
  # Damages every frame (red channel tracks iTime), so render == animation.
  animatedShader = pkgs.writeText "animated.frag" ''
    void mainImage(out vec4 fragColor, in vec2 fragCoord) {
        fragColor = vec4(fract(iTime), 0.25, 0.5, 1.0);
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-wallpaper-render-rate";
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
        wallpaper = { type = "shader"; source = animatedShader; };
        greeterCommand = "${pkgs.writeShellScript "rate-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
      };

      # The render-counter liveness line (→ kmsg); devkmsg=on lifts the
      # kmsg ratelimit so every line lands.
      systemd.services.halmasuit.environment.HALMASUIT_LIVENESS_INTERVAL_MS = "100";
      boot.kernelParams = [ "printk.devkmsg=on" ];

      virtualisation.qemu.options = [
        "-vga none"
        "-device virtio-gpu-pci"
      ];

      users.users.halmasuit-greeter = {
        isSystemUser = true; uid = 999; group = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true; uid = 998; group = "halmasuit-greeter";
      };
    };

  testScript = ''
    import re
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service", timeout=120)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF scanout_active", timeout=120
    )

    def latest_frames():
        out = machine.succeed(
            "dmesg | grep -oE 'halmasuit-shutdown-liveness pid=[0-9]+ frames=[0-9]+' "
            "| tail -1 || true"
        )
        m = re.search(r"frames=(\d+)", out)
        return int(m.group(1)) if m else 0

    # Let the animated wallpaper render, then measure frame_counter
    # advancement over a 5s wall-clock window.
    machine.wait_until_succeeds(
        "dmesg | grep -q halmasuit-shutdown-liveness", timeout=30
    )
    f0 = latest_frames()
    time.sleep(5)
    f1 = latest_frames()
    rate = (f1 - f0) / 5.0
    print(f"visual-wallpaper-render-rate: measured {rate:.1f} renders/sec "
          f"({f1 - f0} frames in 5s)")

    assert f1 > f0, (
        f"render counter did not advance ({f0}->{f1}) — the animated shader "
        f"is not rendering at all in this VM."
    )
    # The vblank-driven fix (Epic #53) must render an animated wallpaper at
    # ~display refresh. The fixed 100 ms timer caps it at ~10 fps.
    assert rate >= 50.0, (
        f"render rate {rate:.1f} fps is below the display refresh — the "
        f"animated wallpaper is THROTTLED by the fixed-interval timer, not "
        f"vblank-driven. (~10 fps = the 100 ms timer; RED until the "
        f"vblank-driven render lands.)"
    )
    print("visual-wallpaper-render-rate: PASS — animated wallpaper renders "
          "at ~display refresh")
  '';
}
