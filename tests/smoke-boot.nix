# Smoke test: boot a NixOS VM with gnomon's actual desktop stack
# (greetd + DankGreeter + niri + DMS via the dms-niri module from
# nix-config), reach graphical.target, and confirm the greeter UI is
# alive.
#
# No frame capture, no flash detection. Just proves the test
# infrastructure works against the real stack. The red-by-design
# transition tests arrive in follow-up tasks.

{
  system,
  nixpkgs,
  nix-config,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true; # niri-flake's pinned niri-unstable may pull unfree deps transitively.
  };

  # The dms-niri module expects `inputs` in its module args. Pass through
  # nix-config's own resolved inputs so niri-flake.nixosModules.niri and
  # inputs.dms.nixosModules.* resolve to the same revisions gnomon runs.
  testInputs = nix-config.inputs // {
    inherit nix-config;
  };
in
pkgs.testers.runNixOSTest {
  name = "smoke-boot";

  # Provide `inputs` to the imported dms-niri module via specialArgs (the
  # supported way to inject module args from outside; `_module.args` from
  # inside the same evaluation triggers infinite recursion).
  node.specialArgs = { inputs = testInputs; };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        "${nix-config}/modules/desktop/dms-niri.nix"
      ];

      desktop.dms-niri.enable = true;

      # Greeter expects a real user to be able to log in. Password "test"
      # so the test driver can later send credentials in follow-up tasks.
      users.users.test = {
        isNormalUser = true;
        password = "test";
        uid = 1000;
        extraGroups = [ "wheel" "video" "input" ];
      };

      # Make sudo passwordless for the test user — the test framework
      # uses sudo to inspect process state inside the VM.
      security.sudo.wheelNeedsPassword = false;

      # VM hardware shape. virtio-gpu-pci gives niri a DRM device to find,
      # but vanilla virtio-gpu has no GBM allocator and niri logs "no
      # allocator available for device" — meaning the greeter UI never
      # actually paints inside the VM. Screenshots are solid black.
      #
      # This is acceptable for v1: the flash we're detecting is a
      # compositor PID change at login, not a visual transition. PID
      # tracking works whether or not pixels render. Visual capture for
      # human-review screenshots is a v1.5 concern (likely needs
      # virtio-vga-gl + an explicit GL display backend, which conflicts
      # with the NixOS test framework's default headless display).
      virtualisation = {
        memorySize = 2048;
        cores = 2;
        diskSize = 4096;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    # 1. Boot to graphical.target.
    machine.start()
    machine.wait_for_unit("graphical.target")

    # 2. greetd is up and the dms-greeter launch chain is alive:
    #    wrapper script → nested niri compositor → Quickshell.
    machine.succeed("systemctl is-active greetd")
    machine.wait_until_succeeds("pgrep -f dms-greeter")
    machine.wait_until_succeeds("pgrep -x niri")
    machine.wait_until_succeeds("pgrep -f quickshell")

    # 3. Capture a screenshot for the test record. It will be solid black
    #    on virtio-gpu-pci because niri's smithay TTY backend has no GBM
    #    allocator (see comment on virtualisation.qemu.options). The
    #    flash-detection test does not depend on this image — it tracks
    #    PID continuity, which works regardless of whether pixels render.
    machine.screenshot("greeter")

    print("smoke-boot: greeter stack is up (greetd + dms-greeter + niri + quickshell)")
  '';
}
