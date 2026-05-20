# tests/visual-frame-callbacks.nix — convergence epic R2: regression
# test for `wl_surface.frame` callback emission.
#
# The Wayland spec (Appendix A, `wl_surface::frame`) requires servers
# to send notifications "so that a client will not send excessive
# updates, while still allowing the highest possible update rate for
# clients that wait for the reply before drawing again." Mesa's
# `dri2_wl_surface_throttle` (in `libEGL_mesa`) is exactly such a
# client: it requests `wl_surface.frame` callbacks and blocks the next
# `eglSwapBuffers` until the callback arrives. Without server-side
# emission, ANY Mesa-EGL client wedges on its second swap.
#
# The gate: niri (Mesa-EGL via smithay-winit) runs as halmasuit's
# greeter — i.e., a Wayland-EGL client of halmasuit. With the bug,
# niri commits 1-3 buffers and parks in `dri2_wl_surface_throttle`
# forever; halmasuit emits 1-3 `frame_rendered` events (its own
# re-renders triggered by niri's commits) and then nothing. With the
# fix, niri keeps drawing and halmasuit keeps re-compositing — the
# `frame_rendered` stream is continuous.
#
# This is a Wayland-EGL-client wedge test, NOT an auth/session test
# (visual-niri-session covers that). No broker drive, no alice user,
# no real-pam_unix — just halmasuit + niri-as-greeter long enough to
# expose the throttle.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };

  # Unmodified upstream niri — the same software-render setup as
  # visual-niri-session (llvmpipe forced; no DRI3).
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;

  niriConfig = pkgs.writeText "niri-config.kdl" ''
    input { keyboard { xkb {} } }
    output "*" {}
    layout {}
    animations { off }
  '';

  greeterCmd = pkgs.writeShellScript "halmasuit-niri-greeter" ''
    # niri is a COMPOSITOR; needs its own writable XDG_RUNTIME_DIR for
    # its own listening socket (even when we don't run any clients
    # under it). halmasuit's /run/halmasuit is the compositor uid's
    # runtime dir — niri must NOT bind there.
    export XDG_RUNTIME_DIR=/run/halmasuit-niri
    # Absolute-path WAYLAND_DISPLAY = libwayland reads as an absolute
    # socket path, so niri reaches halmasuit upstream without dir
    # collision.
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    # Headless software GL (llvmpipe). Same env as visual-niri-session.
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    exec ${niri}/bin/niri --config ${niriConfig}
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-frame-callbacks";

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
        package         = halmasuit; # halmasuit-debug via flake (frame_audit events)
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        witnessImage    = ./fixtures/witness.png;
        greeterCommand  = "${greeterCmd}";
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

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        # niri runs as halmasuit-greeter (uid 999) — its
        # XDG_RUNTIME_DIR must be greeter-writable.
        "d /run/halmasuit-niri 0700 halmasuit-greeter halmasuit-greeter -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

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
    import os
    import sys
    import time

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    # halmasuit has reached steady scanout and spawned niri as its
    # greeter — niri is the foreground Wayland-EGL client.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )
    machine.wait_until_succeeds("pgrep -x niri", timeout=30)
    # niri's first commit reaches halmasuit (foreground swap).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF "
        "'xdg_toplevel mapped as fullscreen foreground'",
        timeout=30,
    )

    # SAMPLING WINDOW: 5 seconds for niri to render frames against
    # halmasuit. With the wedge, niri parks in dri2_wl_surface_throttle
    # after 1-2 commits and the stream stops. With the fix, niri
    # renders continuously (its idle backdrop), halmasuit re-composites
    # on each commit, and frame_rendered fires continuously.
    # Let niri's initial render burst complete. With the wedge, niri
    # commits 1-3 buffers and parks in dri2_wl_surface_throttle; without
    # it, niri renders many frames as the callback cycle releases each
    # swap. (niri naturally idles once its initial setup is steady —
    # no clients of its own, no animations — which is correct
    # compositor behavior. The CONTRACT under test is "no wedge before
    # the third swap," NOT "rendering forever.")
    time.sleep(5)

    frame_events = [
        e for e in visual.introspect_events(machine)
        if e["event"] == "frame_rendered"
    ]
    total = len(frame_events)
    print(f"frame_rendered events at 5s after niri started: {total}")

    # Pre-fix: 1-3 events (wedge at second swap). Post-fix: dozens to
    # hundreds. 60 is comfortably above the wedge ceiling and below
    # any reasonable post-fix floor.
    assert total >= 60, (
        f"wl_surface.frame callbacks not firing: only {total} "
        f"frame_rendered events 5s after niri started (expected ≥60). "
        f"Mesa-EGL clients are wedging in dri2_wl_surface_throttle."
    )

    # Sanity: niri is still alive (not crashed during the window).
    machine.succeed("pgrep -x niri")

    # No black/uncovered/degenerate frame across the window — the
    # witness threads continuously underneath. The convergence work
    # must NOT regress the no-flash invariant.
    visual.assert_no_flash_stream(machine)

    print("visual-frame-callbacks: ALL ASSERTIONS PASSED")
  '';
}
