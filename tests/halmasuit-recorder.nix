# tests/halmasuit-recorder.nix — pure-eval gate for
# `services.halmasuit.diagnostic.recordFrames`.
#
# Contract:
# - recordFrames set, halmasuit-debug (frame_audit build): both units
#   export HALMASUIT_RECORD_FRAMES=<dir>; the rootfs unit lists <dir> in
#   ReadWritePaths; both units create <dir> as mode 0700 owned by
#   compositorUid in an ExecStartPre (the rootfs one `+`-prefixed).
# - recordFrames set, halmasuit (no frame_audit build): evaluation fails
#   through a config.assertions entry whose message names recordFrames.
# - recordFrames null: each unit's environment and serviceConfig equal the
#   set unit's with exactly the recorder's entries removed
#   (HALMASUIT_RECORD_FRAMES, the directory-creating ExecStartPre, rootfs
#   ReadWritePaths), and neither unit carries the recorder directory.
#
# Implementation: synthetic nixosSystem evaluations of the module in its
# rootfs (enable) and boot-from-initrd (fromInitrd.enable) shapes. No VM,
# nothing built beyond evaluation.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-debug,
  halmasuit-session,
  halmasuit-luks,
}:

let
  pkgs = import nixpkgs { inherit system; };
  lib = pkgs.lib;

  recordDir = "/var/lib/halmasuit/frames";
  compositorUid = 998;

  evalWith = { shape, package, diagnostic }: nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ../nix/module.nix
      ({ ... }: {
        services.halmasuit = {
          package           = package;
          session.package   = halmasuit-session;
          luks.package      = halmasuit-luks;
          greeterUid        = 999;
          greeterGroup      = "halmasuit-greeter";
          compositorUid     = compositorUid;
          inherit diagnostic;
        } // (
          if shape == "rootfs"
          then { enable = true; }
          else { fromInitrd.enable = true; }
        );

        users.users.halmasuit-greeter = {
          isSystemUser = true;
          uid          = 999;
          group        = "halmasuit-greeter";
        };
        users.groups.halmasuit-greeter.gid = 999;
        users.users.halmasuit-compositor = {
          isSystemUser = true;
          uid          = compositorUid;
          group        = "halmasuit-greeter";
        };

        # Required-option stubs. Nothing reads these.
        fileSystems."/" = { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
        boot.loader.systemd-boot.enable = true;
        system.stateVersion = "26.05";
      })
    ];
  };

  rootfsWith = package: diagnostic: (evalWith { shape = "rootfs"; inherit package diagnostic; }).config;
  initrdWith = package: diagnostic: (evalWith { shape = "initrd"; inherit package diagnostic; }).config;

  rootUnit  = cfg: cfg.systemd.services.halmasuit;
  initrdUnit = cfg: cfg.boot.initrd.systemd.services.halmasuit;

  # ExecStartPre is a single string on the rootfs unit and a list on the
  # initramfs unit.
  preCmds = unit: lib.toList (unit.serviceConfig.ExecStartPre or [ ]);

  createsDir = cmd:
    lib.hasInfix "install -d -m 0700 -o ${toString compositorUid}" cmd
    && lib.hasSuffix " ${recordDir}" cmd;

  recordFramesFailure = a: !a.assertion && lib.hasInfix "recordFrames" a.message;

  rootSet      = rootfsWith halmasuit-debug { recordFrames = recordDir; };
  initrdSet    = initrdWith halmasuit-debug { recordFrames = recordDir; };
  rootSetPlain = rootfsWith halmasuit { recordFrames = recordDir; };

  rootNull     = rootfsWith halmasuit-debug { recordFrames = null; };
  initrdNull   = initrdWith halmasuit-debug { recordFrames = null; };

  # The set unit with the recorder's own entries removed. Null must equal
  # it exactly, so the option touches nothing else in either unit.
  withoutRecorderEnv = unit: builtins.removeAttrs unit.environment [ "HALMASUIT_RECORD_FRAMES" ];
  rootSetBareConfig = builtins.removeAttrs (rootUnit rootSet).serviceConfig [ "ExecStartPre" "ReadWritePaths" ];
  initrdSetBareConfig = (initrdUnit initrdSet).serviceConfig // {
    ExecStartPre = lib.filter (c: !(createsDir c)) (preCmds (initrdUnit initrdSet));
  };

  checks = [
    {
      name = "set: rootfs environment exports HALMASUIT_RECORD_FRAMES";
      ok = ((rootUnit rootSet).environment.HALMASUIT_RECORD_FRAMES or null) == recordDir;
    }
    {
      name = "set: initramfs environment exports HALMASUIT_RECORD_FRAMES";
      ok = ((initrdUnit initrdSet).environment.HALMASUIT_RECORD_FRAMES or null) == recordDir;
    }
    {
      name = "set: rootfs ReadWritePaths contains the recorder directory";
      ok = lib.elem recordDir ((rootUnit rootSet).serviceConfig.ReadWritePaths or [ ]);
    }
    {
      name = "set: rootfs ExecStartPre creates the directory 0700 owned by compositorUid, + prefixed";
      ok = lib.any (c: lib.hasPrefix "+" c && createsDir c) (preCmds (rootUnit rootSet));
    }
    {
      name = "set: initramfs ExecStartPre creates the directory 0700 owned by compositorUid";
      ok = lib.any createsDir (preCmds (initrdUnit initrdSet));
    }
    {
      name = "set without frame_audit build: evaluation fails naming recordFrames";
      ok = lib.any recordFramesFailure rootSetPlain.assertions;
    }
    {
      name = "set with frame_audit build: no recordFrames assertion fails";
      ok = !(lib.any recordFramesFailure rootSet.assertions);
    }
    {
      name = "null: rootfs environment equals the set one without HALMASUIT_RECORD_FRAMES";
      ok = (rootUnit rootNull).environment == withoutRecorderEnv (rootUnit rootSet);
    }
    {
      name = "null: rootfs serviceConfig equals the set one without ExecStartPre and ReadWritePaths";
      ok = (rootUnit rootNull).serviceConfig == rootSetBareConfig;
    }
    {
      name = "null: initramfs environment equals the set one without HALMASUIT_RECORD_FRAMES";
      ok = (initrdUnit initrdNull).environment == withoutRecorderEnv (initrdUnit initrdSet);
    }
    {
      name = "null: initramfs serviceConfig equals the set one without the directory-creating ExecStartPre";
      ok = (initrdUnit initrdNull).serviceConfig == initrdSetBareConfig;
    }
    {
      name = "null: neither unit exports HALMASUIT_RECORD_FRAMES or creates a recorder directory";
      ok = !((rootUnit rootNull).environment ? HALMASUIT_RECORD_FRAMES)
        && !((initrdUnit initrdNull).environment ? HALMASUIT_RECORD_FRAMES)
        && !(lib.any createsDir (preCmds (rootUnit rootNull)))
        && !(lib.any createsDir (preCmds (initrdUnit initrdNull)));
    }
  ];

  failures = map (c: c.name) (lib.filter (c: !c.ok) checks);

  report =
    if failures == [ ]
    then "halmasuit-recorder: PASS (${toString (builtins.length checks)} checks)"
    else throw "halmasuit-recorder: FAIL: ${lib.concatStringsSep "; " failures}";
in
pkgs.runCommand "halmasuit-recorder" { } ''
  echo '${report}' > $out
''
