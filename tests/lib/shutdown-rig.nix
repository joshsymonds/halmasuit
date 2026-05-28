# tests/lib/shutdown-rig.nix — shared `nodes.machine` config factory
# for the wallpaper-shutdown matrix cells.
#
# Used by visual-shutdown-{pivot-survival,image,video}.nix. The
# `wallpaper` arg is the only required per-cell variation; optional
# args cover the video cell's decoder package and ReadWritePaths for
# cells that need them (image cell does D-Bus Snapshot writes under
# /run/hsnap, hence the ReadWritePaths slot).
#
# What the rig provides:
#   - imports halmasuit's NixOS module + the shared test-user.nix
#   - services.halmasuit with greeter/compositor uids + the layer-shell
#     test-client as the greeterCommand
#   - alice (uid 1001) + halmasuit-greeter (999) + halmasuit-compositor
#     (998) users/groups
#   - halmasuit-vm-client on PATH for the test driver
#   - HALMASUIT_LIVENESS_INTERVAL_MS=25 (kmsg liveness cadence —
#     well below the ~50 ms post-pivot window before kernel halt)
#   - `printk.devkmsg=on` kernel param (removes /dev/kmsg ratelimit
#     so every liveness write lands inside the tight post-pivot slice)
#   - systemd-boot + canTouchEfiVariables=false + GRUB disabled (the
#     useBootLoader=true chroot-install path can't write EFI NVRAM)
#   - virtualisation: useBootLoader/useEFIBoot/mountHostNixStore=false
#     for production-faithful disk layout (#60 regression check —
#     halmasuit's mmaps need /nix/store on the root fs, not 9p)
#
# What stays in each cell .nix:
#   - the `pkgs` / `niri` / `niriConfig` / `sessionCmd` bindings
#     (each cell names them after itself for journal/log clarity)
#   - the `testScript` body (calls into shutdown_testscript.run)
#   - cell-specific fixtures (videoFixture, fallbackFixture)
#   - `system.extraDependencies` for closure-completeness with
#     useBootLoader=true (sessionCmd, niri, plus cell fixtures)
{
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  halmasuit-layer-shell-test-client,
  wallpaper,
  decoder ? null,
  extraStorePaths ? [ ],
  extraTmpfilesRules ? [ ],
  extraReadWritePaths ? [ ],
}:
{ pkgs, lib, ... }:
{
  imports = [
    ../../nix/module.nix
    ./test-user.nix
  ];

  services.halmasuit = {
    enable          = true;
    package         = halmasuit;
    session.package = halmasuit-session;
    greeterUid      = 999;
    greeterGroup    = "halmasuit-greeter";
    compositorUid   = 998;
    inherit wallpaper;
    greeterCommand = "${pkgs.writeShellScript "halmasuit-shutdown-rig-greeter" ''
      export HALMASUIT_TESTCLIENT_KEYBOARD=1
      export HALMASUIT_TESTCLIENT_LAYER=top
      export HALMASUIT_TESTCLIENT_COLOR=#22DD77
      exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
    ''}";
  } // lib.optionalAttrs (decoder != null) {
    decoder.package = decoder;
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
  # driver (sessionCmd, niri, fixtures) but NOT in a normal NixOS
  # closure path has to be pulled in explicitly. The cell .nix
  # passes those in via extraStorePaths.
  system.extraDependencies = extraStorePaths;

  systemd.tmpfiles.rules = [
    "d /run/halmasuit-niri 0700 alice alice -"
  ] ++ extraTmpfilesRules;

  systemd.services.halmasuit = {
    environment.HALMASUIT_LIVENESS_INTERVAL_MS = "25";
    serviceConfig.ReadWritePaths = extraReadWritePaths;
  };

  boot.kernelParams = [ "printk.devkmsg=on" ];

  # #60 fix: boot from a real disk image (ext4 on virtio-blk), with
  # the entire system closure installed on /. This matches production
  # NixOS layout where /nix/store is a directory on the root
  # filesystem — NOT a separate mount. systemd-shutdown never
  # unmounts /; it only remounts it RO, which the kernel permits
  # even on mmap-busy filesystems. Halmasuit's code-page mappings
  # stay live through the entire shutdown sequence.
  #
  # systemd-boot is REQUIRED here (not the host's GRUB default): the
  # disk image install runs `switch-to-configuration boot` inside
  # `nixos-enter`'s chroot, which has no access to the host
  # firmware's EFI NVRAM. GRUB-EFI's installation step uses
  # `efibootmgr` to write Boot#### entries to NVRAM; in a chroot
  # that fails silently, leaving the OVMF firmware with nothing to
  # boot. systemd-boot bypasses NVRAM entirely — it writes a
  # fallback /EFI/BOOT/BOOTX64.EFI which OVMF finds via its built-in
  # removable-media boot path.
  boot.loader.systemd-boot.enable      = true;
  boot.loader.efi.canTouchEfiVariables = false;
  boot.loader.grub.enable              = lib.mkForce false;

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
}
