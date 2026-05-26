# tests/lib/phase-b-golden.nix — shared NixOS-config builder for the
# Phase B golden-boot test matrix (Epic #35).
#
# Returns a NixOS module function the per-cell tests
# (visual-phase-b-{enc,side}-{image,shader,video}.nix) consume via:
#
#     imports = [
#       (import ./lib/phase-b-golden.nix {
#         wallpaper = ...;
#         lukshape  = "side-volume";  # or "encrypted-root" (follow-up)
#         inherit halmasuit-debug halmasuit-luks halmasuit-session
#                 halmasuit-vm-client nix-config;
#         wallpaperStorePaths = [];
#       })
#     ];
#
# All six tests share THIS file's wiring:
#   - services.halmasuit.fromInitrd.enable = true (real Phase B
#     deployment shape; the same one gnomon will run).
#   - DankGreeter (DMS Quickshell) as `greeterCommand`, configured
#     via XDG_DATA_DIRS to discover ONLY our test session entry —
#     so DMS's session-selection picks our wrapped niri without us
#     needing to override the upstream niri.desktop.
#   - niri as the session command, wrapped to set the test-headless
#     env (LIBGL_ALWAYS_SOFTWARE, llvmpipe).
#   - alice (uid 1001, password "testpassword") as the PAM user.
#   - halmasuit-luks's `--passphrase-from` path armed (the
#     non-interactive responder); the LUKS volume is unlocked via
#     the production wire (initramfs systemd-cryptsetup → agent →
#     volume).
#   - boot.loader.systemd-boot + emptyDiskImage [64] + bootSize 2048
#     for the LUKS specialisation dance (first boot luksFormats,
#     switches default, crashes; second boot mounts).

{
  wallpaper,
  lukshape ? "side-volume",
  halmasuit-debug,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
  # Only the video-wallpaper cell consumes this; the other cells pass
  # null and the option default is left untouched. Optional so the
  # image/shader cells aren't forced to build a decoder closure they
  # never exercise.
  halmasuit-decoder ? null,
  nix-config,
  wallpaperStorePaths ? [ ],
}:

{ config, lib, pkgs, ... }:

let
  inherit (pkgs) system;

  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;
  dmsShell = nix-config.inputs.dms.packages.${system}.dms-shell;
  dmsQuickshell = nix-config.inputs.dms.packages.${system}.quickshell;

  # Minimal niri config — empty workspace, no animations, no
  # autostart. Same shape as tests/visual-niri-session.nix.
  niriConfig = pkgs.writeText "phase-b-niri-config.kdl" ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    output "*" {
    }

    layout {
    }

    animations {
        off
    }
  '';

  # The session command the broker forks-then-drops as alice. niri
  # is nested as a Wayland client of halmasuit (WAYLAND_DISPLAY set
  # ⇒ niri's winit backend).
  niriCmd = pkgs.writeShellScript "phase-b-niri-session" ''
    # niri needs its own XDG_RUNTIME_DIR — owned by the authed
    # session user — because it binds its own listening socket
    # there. halmasuit's /run/halmasuit/ is owned by the compositor
    # uid and would EPERM (provisioned via tmpfiles below).
    export XDG_RUNTIME_DIR=/run/halmasuit-niri
    # libwayland treats a '/'-containing WAYLAND_DISPLAY as an
    # absolute socket path; niri reaches halmasuit upstream this
    # way without colliding with its own /run/halmasuit-niri/wayland-1.
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    # Headless virtio-gpu-pci has no working GL/EGL; force llvmpipe
    # for niri's smithay GLES renderer.
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    exec ${niri}/bin/niri --config ${niriConfig}
  '';

  # Custom Phase B wayland-session .desktop file. DMS's
  # GreeterContent.qml discovers .desktop files by scanning each
  # entry of XDG_DATA_DIRS for `share/wayland-sessions/*.desktop`
  # and indexes the FIRST one as `selectedSession[0]` (no other
  # selection signal in headless — no saved session, no UI nav).
  # By putting OUR dir first in XDG_DATA_DIRS (greeter wrapper
  # below) and provisioning only this one file, DMS picks our
  # wrapped niri as the default session — no niri.desktop override,
  # no upstream patch.
  testSessions = pkgs.writeTextDir "share/wayland-sessions/phase-b-niri.desktop" ''
    [Desktop Entry]
    Name=phase-b-niri
    Comment=Phase B test niri session (Epic #35 golden-boot)
    Exec=${niriCmd}
    Type=Application
  '';

  # DankGreeter wrapper. Same env shape as
  # tests/visual-dankgreeter-auth.nix's greeterCmd, except:
  #   - GREETD_SOCK points at the abstract socket halmasuit binds
  #     in the fromInitrd deployment (@halmasuit-greetd).
  #   - XDG_DATA_DIRS prepends testSessions so DMS sees our session
  #     first.
  greeterCmd = pkgs.writeShellScript "phase-b-dankgreeter" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    # halmasuit's spawn_greeter already exports GREETD_SOCK pointing
    # at the path it bound — we don't need to override it. The
    # production fromInitrd module pins this to
    # `/run/halmasuit/greetd.sock` so Quickshell's greetd client
    # (which doesn't interpret a leading '@' as the abstract namespace)
    # connects via the standard filesystem path.
    # OUR test sessions dir FIRST so DMS picks phase-b-niri.desktop.
    export XDG_DATA_DIRS=${testSessions}/share:/run/current-system/sw/share:/usr/share
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    export QT_QPA_PLATFORM=wayland
    export QT_QUICK_BACKEND=software
    export QT_WAYLAND_DISABLE_WINDOWDECORATION=1
    export HOME=/run/halmasuit-greeter
    export XDG_CACHE_HOME=$HOME/.cache
    export DMS_RUN_GREETER=1
    mkdir -p "$XDG_CACHE_HOME/dms-greeter"
    exec ${dmsQuickshell}/bin/quickshell \
      -p ${dmsShell}/share/quickshell/dms
  '';

  # The LUKS passphrase the side-volume tests use. NOT a secret —
  # the volume is created at test time with this exact string. The
  # passphraseFile is baked into the initramfs closure (the module
  # warns this exposes /nix/store world-readable, which is fine for
  # a test fixture).
  passphraseFile = pkgs.writeText "phase-b-luks-passphrase" "luks-test-unlock-secret";

  # Phase B side-volume LUKS-unlock ExecStart script. Bound outside
  # the unit so it lives in a known nix-store path which we add to
  # `boot.initrd.systemd.storePaths` below — otherwise the script
  # exists on the host filesystem but NOT in the initramfs closure,
  # and systemd's ExecStart fails 203/EXEC.
  cryptsetupAttachScript = pkgs.writeShellScript "phase-b-attach-vdb" ''
    echo "phase-b: ExecStart entered"
    # Wait for /dev/vdb (udev race; even though we order
    # After=dev-vdb.device below, it can lag).
    for i in $(seq 1 50); do
      if [ -e /dev/vdb ]; then break; fi
      ${pkgs.coreutils}/bin/sleep 0.1
    done
    if [ ! -e /dev/vdb ]; then
      echo "phase-b: /dev/vdb still missing after 5s wait; skipping"
      exit 0
    fi
    if ! ${pkgs.cryptsetup}/bin/cryptsetup isLuks /dev/vdb; then
      echo "phase-b: /dev/vdb not yet LUKS; skipping attach (first-boot OK)"
      exit 0
    fi
    # systemd-cryptsetup doesn't autoload dm_crypt + cipher modules.
    # NixOS's boot.initrd.luks.devices adds them to
    # availableKernelModules (initramfs closure) but not to the
    # force-load list, so the device-mapper crypt target type isn't
    # registered when systemd-cryptsetup calls dm-reload. Explicitly
    # modprobe before attach. The names are NixOS-shipped kernel
    # modules; their absence would surface as a build-time error.
    echo "phase-b: loading dm_crypt + cipher modules"
    ${pkgs.kmod}/bin/modprobe dm_crypt || echo "phase-b: dm_crypt modprobe failed (continuing)"
    ${pkgs.kmod}/bin/modprobe aes || echo "phase-b: aes modprobe failed (continuing)"
    ${pkgs.kmod}/bin/modprobe xts || echo "phase-b: xts modprobe failed (continuing)"
    ${pkgs.kmod}/bin/modprobe sha256 || echo "phase-b: sha256 modprobe failed (continuing)"
    echo "phase-b: /dev/vdb IS LUKS; calling systemd-cryptsetup attach"
    exec ${pkgs.systemd}/bin/systemd-cryptsetup attach test-luks-data /dev/vdb
  '';
in
{
  imports = [
    nix-config.inputs.niri-flake.nixosModules.niri
    nix-config.inputs.dms.nixosModules.greeter
  ];

  # Niri is the session command (wrapped above). Module-managed so
  # niri.desktop ends up under /run/current-system/sw/share/wayland-
  # sessions/ — even though DMS picks our custom phase-b-niri.desktop
  # via XDG_DATA_DIRS priority, niri's own runtime + system deps still
  # need to be available.
  programs.niri.enable = true;
  programs.niri.package = niri;

  # DankGreeter (DMS Quickshell). compositor.name is metadata that
  # appears in DMS's About tab UI (not used for session selection,
  # which comes from the .desktop file at index 0 of the
  # XDG_DATA_DIRS scan — see testSessions above). DMS constrains
  # the value to a fixed enum, so we pick "niri" — accurate since
  # our test session DOES wrap upstream niri.
  programs.dank-material-shell.greeter = {
    enable = true;
    compositor.name = "niri";
  };

  # The DMS greeter module turns on `services.greetd` which spawns
  # its OWN nested niri running Quickshell. We don't want that —
  # halmasuit IS the compositor; DMS Quickshell runs directly as
  # halmasuit's `greeterCommand` (see greeterCmd above). Force greetd
  # off; the default_session.user assignment is for the DMS module
  # which still reads that field even when greetd itself is off
  # (mirrors visual-dankgreeter-auth.nix's pattern).
  services.greetd.enable = lib.mkForce false;
  services.greetd.settings.default_session.user = "halmasuit-greeter";

  # halmasuit Phase B fromInitrd deployment. Wallpaper config is the
  # parametric input that distinguishes the six cells.
  #
  # ESP-fit gating: encrypted-root cells use a systemd-boot
  # specialisation (the `cryptroot` entry), which means the ESP has
  # to hold TWO initramfs entries — and the fromInitrd initramfs is
  # large (~250 MiB; mesa + libglvnd + xkbcommon + halmasuit-debug +
  # halmasuit-luks closure). NixOS pins the ESP at 256 MiB and the
  # qemu-vm module doesn't expose `bootSize` to override that. So for
  # encrypted-root we DISABLE fromInitrd in the BASE config (small
  # initramfs — first boot just needs to luksFormat /dev/vdb and
  # switch the bootloader default) and let the specialisation below
  # `mkForce` it on (full Phase B initramfs).
  # For encrypted-root we have fromInitrd off in BASE (see comment
  # below), but the DMS greeter module asserts at eval time that the
  # `halmasuit-greeter` user it references in
  # `services.greetd.settings.default_session.user` exists. The
  # halmasuit module auto-creates this user when enable / fromInitrd /
  # session is on; with none of those set in BASE on encrypted-root,
  # we need to wire the user manually so DMS's assertion passes.
  users.users.halmasuit-greeter = lib.mkIf (lukshape == "encrypted-root") {
    isSystemUser = true;
    group = "halmasuit-greeter";
    uid = 999;
  };
  users.groups.halmasuit-greeter = lib.mkIf (lukshape == "encrypted-root") {
    gid = 999;
  };

  services.halmasuit = {
    fromInitrd.enable = lukshape != "encrypted-root";
    package = halmasuit-debug;
    luks.package = halmasuit-luks;
    luks.passphraseFile = passphraseFile;
    session.package = halmasuit-session;
    inherit wallpaper;
    greeterCommand = "${greeterCmd}";
    # Only the video cell ships a decoder package; the option default
    # would try to resolve `pkgs.halmasuit-decoder` (not in pkgs in
    # this test eval) on the image/shader paths, so we ONLY set this
    # when the test cell wires a decoder closure.
    decoder = lib.mkIf (halmasuit-decoder != null) { package = halmasuit-decoder; };
    # Default uids: greeter=999, compositor=998.
  };

  # Bake everything halmasuit needs post-pivot into the rootfs
  # nix-store. The fromInitrd deployment chroots into rootfs's
  # process-root via the broker root-fd handoff; post-chroot
  # halmasuit looks up paths there, not in the initramfs view.
  system.extraDependencies = [
    niri
    dmsShell
    dmsQuickshell
    testSessions
    niriCmd
    greeterCmd
  ] ++ wallpaperStorePaths;

  # Alice — the real PAM-authenticatable user. uid 1001 is the
  # convention these visual tests use (visual-dankgreeter-auth +
  # visual-niri-session both pin it there). Member of
  # halmasuit-greeter so niri (running AS alice post-auth) can
  # reach halmasuit's 0660 wayland socket.
  users.users.alice = {
    isNormalUser = true;
    uid = 1001;
    group = "alice";
    password = "testpassword";
    extraGroups = [ "halmasuit-greeter" ];
  };
  users.groups.alice.gid = 1001;

  systemd.tmpfiles.rules = [
    # halmasuit-debug writes Snapshot() PNGs here.
    "d /run/hsnap 0777 root root -"
    # DankGreeter's HOME / XDG_RUNTIME_DIR — owned by the greeter
    # uid so Quickshell can write its cache.
    "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
    # niri's own XDG_RUNTIME_DIR — owned by alice for niri's
    # listening socket.
    "d /run/halmasuit-niri 0700 alice alice -"
  ];

  # niri.desktop needs to be discoverable via XDG_DATA_DIRS for the
  # niri-flake module's full session integration; testSessions
  # provides ours first in priority order.
  environment.pathsToLink = [ "/share/wayland-sessions" ];

  environment.systemPackages = [ halmasuit-vm-client pkgs.cryptsetup ];

  # ── LUKS wiring ───────────────────────────────────────────────────
  #
  # Two shapes; the wiring diverges by shape:
  #
  # `side-volume` (current cells):
  #   /dev/vdb is a non-root LUKS volume. Rootfs is the default
  #   qcow2-backed /dev/vda. SAME-CONFIG dual-boot pattern (no
  #   specialisation): first boot luksFormats /dev/vdb (no LUKS header
  #   yet, the custom unit's `cryptsetup isLuks` guard skips), reboots;
  #   second boot's custom unit ticks systemd-cryptsetup which asks
  #   for a passphrase; halmasuit-luks responds.
  #
  # `encrypted-root` (enc-* cells):
  #   The rootfs itself is on a LUKS volume (/dev/vdb formatted on
  #   first boot, then specialisation `cryptroot` swaps
  #   `virtualisation.rootDevice` to `/dev/mapper/cryptroot` and
  #   `autoFormat`s it). systemd-cryptsetup is pulled into the
  #   initramfs target chain automatically (cryptroot is REQUIRED for
  #   /) so no custom unit is needed. halmasuit-luks's ask-password
  #   responder still answers the prompt. Dual-boot dance: first boot
  #   runs in the default (non-cryptroot) config, luksFormats /dev/vdb,
  #   `bootctl set-default ...cryptroot.conf`, crashes; second boot
  #   activates the specialisation and unlocks the rootfs.
  #
  # Force-load the LUKS cipher chain in initramfs. luksroot.nix adds
  # dm_crypt et al. to availableKernelModules (initramfs CAN load
  # them) but not to kernelModules (WILL load them). On the
  # encrypted-root path the rootfs mount blocks until dm_crypt is
  # available — without this the kernel-modules autoload race makes
  # the unlock unit transiently fail. Declaring them in
  # `boot.initrd.kernelModules` makes systemd-modules-load force-load
  # them deterministically. Used in BOTH shapes for the same reason
  # (the side-volume custom unit also relies on the modules being
  # already-loaded; without this its modprobe calls fail).
  boot.initrd.kernelModules = [
    "dm_crypt"
    "aes"
    "xts"
    "sha256"
  ];

  # Side-volume LUKS unlock — only when lukshape says so.
  #
  # We DON'T use `boot.initrd.luks.devices.<name>` for the unlock
  # itself — that module's systemd-stage1 wiring
  # (nixos/modules/system/boot/luksroot.nix:1061) ships /etc/crypttab
  # + the cryptsetup generator into the initramfs, but the generated
  # systemd-cryptsetup@<name>.service isn't pulled into NixOS's
  # initrd.target chain by default for NON-ROOT volumes
  # (cryptsetup.target is upstream-wantedBy=sysinit.target — the
  # rootfs chain), so the unit never wakes up. We tried adding it via
  # `boot.initrd.systemd.targets.initrd.wants` but it still didn't
  # take effect (probably because the conditional crypttab placement
  # in luksroot.nix has its own internal gates that don't fire under
  # the systemd-initrd shape we use).
  #
  # Instead: provision a small initramfs systemd unit that runs
  # `systemd-cryptsetup attach test-luks-data /dev/vdb` as a oneshot,
  # ordered AFTER halmasuit-luks is ready and BEFORE
  # initrd-switch-root. systemd-cryptsetup inside a managed unit
  # ticks the password-agent loop correctly. halmasuit-luks answers;
  # the volume unlocks BEFORE pivot, so `/dev/mapper/test-luks-data`
  # is present at multi-user.target.
  boot.initrd.systemd.services = lib.optionalAttrs (lukshape == "side-volume") {
    "phase-b-cryptsetup-test-luks-data" = {
      description = "Phase B side-volume LUKS unlock (driven by halmasuit-luks)";
      wantedBy = [ "initrd.target" ];
      before = [ "initrd-switch-root.service" ];
      requires = [ "halmasuit-luks.service" ];
      after = [ "halmasuit-luks.service" "systemd-udev-settle.service" "dev-vdb.device" ];
      wants = [ "dev-vdb.device" ];
      unitConfig = {
        DefaultDependencies = false;
        IgnoreOnIsolate = true;
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        StandardOutput = "journal";
        StandardError = "journal";
        ExecStart = "${cryptsetupAttachScript}";
      };
    };
  };

  # Bundle the attach script + cryptsetup binary + kmod (modprobe)
  # into the initramfs closure. systemd's ExecStart resolves paths
  # against the initramfs filesystem; without this the unit fails
  # 203/EXEC. Side-volume only — the encrypted-root path uses the
  # upstream systemd-cryptsetup generator (cryptroot is the rootfs,
  # so the unit IS pulled into the initramfs target chain by the /
  # mount dependency); cryptsetup + systemd-cryptsetup ship via the
  # luksroot module's storePaths in that case.
  boot.initrd.systemd.storePaths =
    lib.optionals (lukshape == "side-volume") [
      "${cryptsetupAttachScript}"
      "${pkgs.cryptsetup}/bin/cryptsetup"
      "${pkgs.systemd}/bin/systemd-cryptsetup"
      "${pkgs.kmod}/bin/modprobe"
    ];

  # Side-volume only: declare boot.initrd.luks.devices to trigger
  # NixOS's luksroot module's automatic module-pull. We don't depend
  # on the generator's unit running — our custom unit above does the
  # actual attach. The declaration is here purely for its side
  # effect: the right modules end up in the initramfs
  # availableKernelModules + kernelModules lists.
  #
  # Encrypted-root: the LUKS declaration lives in the specialisation
  # block below, so the FIRST boot (default specialisation) has no
  # LUKS to unlock; the testScript luksFormats /dev/vdb in that
  # window and switches boot default to the cryptroot specialisation.
  boot.initrd.luks.devices = lib.optionalAttrs (lukshape == "side-volume") {
    "test-luks-data" = {
      device = "/dev/vdb";
      allowDiscards = false;
    };
  };

  # ── Encrypted-root specialisation ─────────────────────────────────
  #
  # Activated by the testScript via `bootctl set-default
  # ...cryptroot.conf` after first-boot luksFormat. Inherits the
  # base config (halmasuit + halmasuit-luks + Phase B wiring), adds
  # only the LUKS-rootfs bits. autoFormat lets NixOS format the
  # cryptroot device with ext4 on first activation, then mounts it
  # at /. The systemd-cryptsetup unit for `cryptroot` IS in the
  # initramfs target chain (cryptroot is required-for /) so the
  # unlock fires automatically; halmasuit-luks answers the prompt.
  specialisation = lib.optionalAttrs (lukshape == "encrypted-root") {
    cryptroot.configuration = {
      # `mkForce` the gate so the specialisation initramfs ships the
      # Phase B halmasuit closure (base config has it off for ESP
      # reasons — see the comment on `services.halmasuit.fromInitrd`
      # above).
      services.halmasuit.fromInitrd.enable = lib.mkForce true;
      boot.initrd.luks.devices = lib.mkVMOverride {
        cryptroot.device = "/dev/vdb";
      };
      virtualisation.rootDevice = "/dev/mapper/cryptroot";
      virtualisation.fileSystems."/".autoFormat = true;
    };
  };

  virtualisation = {
    memorySize = 4096;
    cores = 2;
    diskSize = 8192;
    emptyDiskImages = [ 64 ];
    useBootLoader = true;
    useEFIBoot = true;
    mountHostNixStore = true;
    qemu.options = [
      "-vga none"
      "-device virtio-gpu-pci"
    ];
  };
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
}
