# tests/visual-gtk4-smoke.nix — R12 (GTK4 half).
#
# The convergence epic R12 calls for Qt6 + GTK4 smoke clients as
# separate VM gates. The Qt6 piece is `visual-dankgreeter` (DMS
# Quickshell, real Qt6/QML stack). This is the GTK4 half: a minimal
# GTK4 wayland client built inline from a tiny C source, launched as
# halmasuit's greeter. Asserts halmasuit can host a real GTK4
# toolkit client end-to-end (registry bind through frame render)
# without breaking the no-flash invariant.
#
# Why GTK4 in C and not Rust:
#  - The smallest reproducible "real toolkit" surface — GTK4's
#    wayland-gtk plugin is what every GTK4 app uses, so this proves
#    the path Qt6 doesn't exercise (different EGL/dmabuf negotiation,
#    different protocol-version preferences).
#  - No Rust gtk4 crate means no transitive workspace dependency on
#    glib/gtk-rs Cargo trees that ripple through halmasuit's lints.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };

  # Tiny GTK4 smoke client. Creates an ApplicationWindow with a
  # labeled drawing area, sets a brand-purple background, stays
  # mapped indefinitely (the test driver kills the VM at teardown
  # so we don't need a self-exit). C is chosen because nixpkgs ships
  # GTK4's pkg-config / dev files and `gcc` builds this in <1s with
  # zero workspace impact.
  gtk4-smoke = pkgs.runCommand "halmasuit-gtk4-smoke" {
    nativeBuildInputs = [ pkgs.pkg-config pkgs.gcc ];
    buildInputs = [ pkgs.gtk4 ];
  } ''
    cat > smoke.c <<'EOF'
    #include <gtk/gtk.h>

    // Bypass GtkApplication entirely. GtkApplication wraps GApplication
    // which requires a session D-Bus to register (even with
    // G_APPLICATION_NON_UNIQUE, newer GTK4 still tries the bus and the
    // failure cascades into never reaching `activate`). For a smoke
    // test under a greeter user with no session bus, the plain
    // gtk_init + GMainLoop path is the minimum that proves "GTK4
    // connects to halmasuit and paints a window."
    int main(int argc, char **argv) {
      (void)argc;
      (void)argv;
      gtk_init();

      GtkWidget *window = gtk_window_new();
      gtk_window_set_title(GTK_WINDOW(window), "halmasuit-gtk4-smoke");
      gtk_window_set_default_size(GTK_WINDOW(window), 1280, 800);

      GtkWidget *label = gtk_label_new("halmasuit GTK4 smoke");
      gtk_label_set_xalign(GTK_LABEL(label), 0.5);
      gtk_label_set_yalign(GTK_LABEL(label), 0.5);
      gtk_widget_set_hexpand(label, TRUE);
      gtk_widget_set_vexpand(label, TRUE);
      gtk_window_set_child(GTK_WINDOW(window), label);

      gtk_window_present(GTK_WINDOW(window));
      g_print("HALMASUIT_GTK4_SMOKE: window_mapped\n");
      // stdout is block-buffered when not a tty; force-flush so the
      // VM driver greps for the marker without waiting for the
      // buffer to fill or the process to exit.
      fflush(stdout);

      GMainLoop *loop = g_main_loop_new(NULL, FALSE);
      g_main_loop_run(loop);
      return 0;
    }
    EOF
    mkdir -p $out/bin
    gcc -Wall -Wextra -o $out/bin/halmasuit-gtk4-smoke smoke.c \
      $(pkg-config --cflags --libs gtk4)
  '';

  greeterLauncher = pkgs.writeShellScript "halmasuit-gtk4-greeter" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export GDK_BACKEND=wayland
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    # GTK4 caches a vulkan/dmabuf pipeline image at $XDG_CACHE_HOME;
    # halmasuit-greeter is a system user with no $HOME, so point the
    # cache at a writable scratch directory inside the per-greeter
    # runtime dir.
    export HOME=/run/halmasuit-greeter
    export XDG_CACHE_HOME=/run/halmasuit-greeter/.cache
    mkdir -p "$XDG_CACHE_HOME/gtk-4.0"
    # GTK4 4.20+ defaults to the GSK Vulkan renderer; the headless
    # VM has no vulkan-capable GPU. Force the GL renderer (llvmpipe-
    # backed) so GTK4 renders without Vulkan ICD failures.
    export GSK_RENDERER=gl
    exec ${gtk4-smoke}/bin/halmasuit-gtk4-smoke
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-gtk4-smoke";

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
        witnessImage    = ./fixtures/witness.png;
        greeterCommand  = "${greeterLauncher}";
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
        "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
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

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )

    # GTK4 client process is running (halmasuit forked it as the
    # tracked greeter child).
    machine.wait_until_succeeds("pgrep -f halmasuit-gtk4-smoke", timeout=30)

    # GTK4's wayland-gdk backend connected to halmasuit and committed
    # its xdg_toplevel through the full xdg-shell protocol surface
    # we wired in this convergence epic. The halmasuit-side trace is
    # the load-bearing assertion (the GTK4-side g_print marker goes
    # to the gtk4-smoke process's own journal unit, not halmasuit's).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )

    # Foreground swapped to greeter (the GTK4 app's xdg-toplevel).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=60
    )

    # No black/uncovered/degenerate frame across boot→witness→GTK4
    # render — the canonical no-flash invariant.
    visual.assert_no_flash_stream(machine)

    print("visual-gtk4-smoke: ALL ASSERTIONS PASSED")
  '';
}
