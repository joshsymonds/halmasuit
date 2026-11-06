# Gnomon lab rescue module. Carries every setting that lets an experimental
# boot on gnomon recover without a physical visit: a hardware watchdog armed
# in both boot stages, a stage-1 deadline, a rootfs deadline, reboot on
# kernel panic, and Wake-on-LAN. Everything is gated on halmasuit.lab.enable.
# The module declares no specialisation and adds no generation or boot entry.
#
# Lab instrumentation settings: netconsole from the initramfs, U2F off for
# listed PAM services, the lab-crc scanout sampler, which runs in stage 1
# and after switch-root, and the lab-vkms-crc vkms sampler, which has its own
# option and runs the same way. The probe option loads uinput and defines
# lab-probe.service, an on-demand root oneshot that nothing starts on its own.
# With probe.earlyKeyboard, the uinput keyboard holder runs from stage 1 across
# switch-root, and lab-probe uses it. Every option defaults off or empty,
# except probe.consumerUser, which defaults to greeter.
#
# Journal contract (frozen; consumed by tests/hw/timeline.py). Each entry is
# one journal line, identified by SYSLOG_IDENTIFIER. `t` is CLOCK_MONOTONIC in
# microseconds. Hex values are 8 lowercase digits.
#
#   lab-crc (SYSLOG_IDENTIFIER=lab-crc; stage-1 unit lab-crc-initrd.service,
#   rootfs unit lab-crc.service):
#     start t=<us>                  once, at process start
#     sample card=<cardN> crtc=<id> t=<us> comp=<hex> rg=<hex> out=<hex> comp_sup=<0|1> rg_sup=<0|1> out_sup=<0|1>
#                                   for a head's first sample, and whenever any
#                                   value or flag differs from that head's
#                                   previous sample
#     error card=<cardN> crtc=<id> t=<us> errno=<n>
#                                   once each time a head's CRC read starts failing
#
#   lab-vkms-crc (SYSLOG_IDENTIFIER=lab-vkms-crc; stage-1 unit lab-vkms-crc-initrd.service,
#   rootfs unit lab-vkms-crc.service; gated on halmasuit.lab.vkmsCrc.enable):
#     start t=<us>                  once, at process start
#     sample card=<cardN> crtc=<id> t=<us> crc=<hex8> active=<0|1>
#                                   for a CRTC's first observation, and whenever its
#                                   crc or active differs from that CRTC's previous
#                                   sample; an inactive CRTC logs active=0 crc=00000000
#     error card=<cardN> crtc=<id> t=<us> errno=<n>
#                                   once each time a CRTC's read starts failing
#
#   lab-probe (SYSLOG_IDENTIFIER=lab-probe; unit lab-probe.service):
#     device node=<path>
#     ready holder=<comm> pid=<pid>
#     typed keys=<n>
#     error reason=<text>
{ config, lib, pkgs, ... }:

let
  cfg = config.halmasuit.lab;

  initrdSystemctl = "${config.boot.initrd.systemd.package}/bin/systemctl";
  systemctl = "${config.systemd.package}/bin/systemctl";

  watchdogManager = {
    WatchdogDevice = "/dev/watchdog0";
    RuntimeWatchdogSec = "60s";
  };

  netconsoleDevice = "sys-subsystem-net-devices-${cfg.netconsole.interface}.device";

  labCrc = pkgs.callPackage ./lab-crc.nix { };

  labVkmsCrc = pkgs.callPackage ./lab-vkms-crc.nix { };

  labKbd = pkgs.callPackage ./lab-probe-kbd.nix { };

  labProbe = ./lab_probe.py;

  earlyKbd = cfg.probe.enable && cfg.probe.earlyKeyboard;
in
{
  options.halmasuit.lab = {
    enable = lib.mkEnableOption "the gnomon lab rescue settings";

    displayManagerUnit = lib.mkOption {
      type = lib.types.str;
      default = "display-manager.service";
      description = ''
        Unit whose activity proves the rootfs came up. If it is not active
        `rootfsDeadline` seconds after boot, the machine reboots.
      '';
    };

    stage1Deadline = lib.mkOption {
      type = lib.types.ints.positive;
      default = 180;
      description = ''
        Seconds after boot by which root must be mounted. The stage-1
        deadline reboots the machine if it is still armed at that point. It
        disarms when `initrd-switch-root.target` starts, which happens only
        after root is mounted.
      '';
    };

    rootfsDeadline = lib.mkOption {
      type = lib.types.ints.positive;
      default = 300;
      description = ''
        Seconds after boot by which `displayManagerUnit` must be active. The
        rootfs deadline reboots the machine otherwise.
      '';
    };

    wolInterface = lib.mkOption {
      type = lib.types.str;
      default = "enp7s0";
      description = "Network interface that answers Wake-on-LAN magic packets.";
    };

    crcSampler.enable = lib.mkEnableOption "the lab-crc scanout CRC sampler";

    vkmsCrc.enable = lib.mkEnableOption "the lab-vkms-crc vkms CRC sampler";

    probe.enable = lib.mkEnableOption "the lab-probe uinput keyboard and probe unit";

    probe.consumerUser = lib.mkOption {
      type = lib.types.str;
      default = "greeter";
      description = "User whose process must hold the keyboard node for the probe to type; greetd's greeter runs as greeter.";
    };

    probe.username = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Username the probe types before the wrong password. Empty types only the wrong password.";
    };

    probe.earlyKeyboard = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Hold the probe's uinput keyboard from stage 1 through switch-root, so it
        exists before the greeter starts. Takes effect only with probe.enable.
      '';
    };

    netconsole = {
      enable = lib.mkEnableOption "netconsole from the initramfs, sending kernel messages over UDP";

      targetIp = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "IPv4 address of the host that receives netconsole messages.";
      };

      sourceIp = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "IPv4 address the lab host sends netconsole messages from.";
      };

      interface = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "Network interface netconsole sends through.";
      };

      port = lib.mkOption {
        type = lib.types.port;
        default = 6666;
        description = "UDP port on targetIp that receives netconsole messages.";
      };
    };

    u2fOffServices = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        PAM services whose U2F authentication the lab image turns off, so a
        host's U2F setting cannot prompt for a token on these services.
      '';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      # Hardware watchdog: stage 1 arms it, and the rootfs keeps it armed.
      boot.initrd.kernelModules = [ "sp5100_tco" ] ++ lib.optional earlyKbd "uinput";
      boot.initrd.systemd.settings.Manager = watchdogManager;
      systemd.settings.Manager = watchdogManager;

      # Reboot on kernel panic, with a 10 second grace period.
      boot.kernelParams = [ "panic=10" ];

      # Unlock timing only. Passphrase prompts give up at half the stage-1
      # deadline, and the FIDO2 token wait falls back to a prompt after 30s.
      boot.initrd.luks.devices.cryptroot.crypttabExtraOpts = [
        "timeout=${toString (cfg.stage1Deadline / 2)}s"
        "token-timeout=30s"
      ];

      # Stage-1 deadline. The timer disarms when initrd-switch-root.target
      # starts, which initrd-cleanup.service does only after root is mounted.
      # DefaultDependencies=no keeps sysinit.target's Conflicts=emergency.target
      # from stopping the units. IgnoreOnIsolate keeps them through isolate.
      boot.initrd.systemd.timers.lab-stage1-deadline = {
        wantedBy = [ "initrd.target" ];
        conflicts = [ "initrd-switch-root.target" ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
        };
        timerConfig.OnBootSec = "${toString cfg.stage1Deadline}s";
      };
      boot.initrd.systemd.services.lab-stage1-deadline = {
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
        };
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${initrdSystemctl} --force reboot";
        };
      };

      # Rootfs deadline. The check service fails when displayManagerUnit is not
      # active, and that failure starts the reboot unit.
      systemd.timers.lab-rootfs-deadline = {
        wantedBy = [ "timers.target" ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
        };
        timerConfig.OnBootSec = "${toString cfg.rootfsDeadline}s";
      };
      systemd.services.lab-rootfs-deadline = {
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
        };
        onFailure = [ "lab-rootfs-reboot.service" ];
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${systemctl} is-active --quiet ${cfg.displayManagerUnit}";
        };
      };
      systemd.services.lab-rootfs-reboot = {
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
        };
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${systemctl} --force reboot";
        };
      };

      # Wake-on-LAN magic packets on the lab interface.
      networking.interfaces.${cfg.wolInterface}.wakeOnLan.enable = true;

      # Netconsole from stage 1. The module loads once the interface device
      # exists, and the unit is Before=initrd.target so switch-root waits for
      # modprobe to finish. The module stays loaded across switch-root. It
      # sends UDP only, and the firewall is not touched.
      assertions = [
        {
          assertion = !cfg.netconsole.enable
            || (cfg.netconsole.targetIp != "" && cfg.netconsole.sourceIp != "" && cfg.netconsole.interface != "");
          message = "halmasuit.lab.netconsole needs targetIp, sourceIp and interface when enabled.";
        }
      ];
      boot.initrd.availableKernelModules = lib.optional cfg.netconsole.enable "netconsole";
      boot.initrd.systemd.storePaths =
        lib.optional cfg.netconsole.enable pkgs.kmod
        ++ lib.optional cfg.crcSampler.enable labCrc
        ++ lib.optional earlyKbd labKbd;
      boot.initrd.systemd.services.lab-netconsole = lib.mkIf cfg.netconsole.enable {
        wantedBy = [ "initrd.target" ];
        before = [ "initrd.target" ];
        bindsTo = [ netconsoleDevice ];
        after = [ netconsoleDevice ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.kmod}/bin/modprobe netconsole netconsole=+@${cfg.netconsole.sourceIp}/${cfg.netconsole.interface},${toString cfg.netconsole.port}@${cfg.netconsole.targetIp}/";
        };
      };

      # CRC sampler. The stage-1 unit starts as early as stage 1 allows and is
      # stopped by the switch-root isolate. The rootfs unit is started before the
      # display manager; the sampler opens the card and drops DRM master within
      # microseconds of starting, long before the greeter compositor opens it.
      # Both restart the sampler when it exits nonzero.
      boot.initrd.systemd.services.lab-crc-initrd = lib.mkIf cfg.crcSampler.enable {
        wantedBy = [ "sysinit.target" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          ExecStart = "${labCrc}/bin/lab-crc";
          SyslogIdentifier = "lab-crc";
          Restart = "on-failure";
        };
      };
      systemd.services.lab-crc = lib.mkIf cfg.crcSampler.enable {
        wantedBy = [ "sysinit.target" ];
        before = [ cfg.displayManagerUnit ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          ExecStart = "${labCrc}/bin/lab-crc";
          SyslogIdentifier = "lab-crc";
          Restart = "on-failure";
        };
      };

      # Keyboard probe. uinput loads at boot so the probe can create its virtual
      # keyboard on demand. lab-probe.service has no wantedBy, requiredBy,
      # upheldBy, timer, path, or socket, so only an explicit start runs it. The
      # runner starts it after the greeter is up. The probe types 32 BackSpace,
      # then the username and Enter when probe.username is set, then a fixed
      # wrong password and Enter. It stores no password.
      #
      # With probe.earlyKeyboard, lab-probe-kbd creates the keyboard in stage 1
      # and holds it through switch-root: the initrd unit keeps the holder running
      # across the pivot, and the rootfs unit adopts it without starting anything.
      # The probe runs with --early-keyboard, so it writes through the holder's
      # FIFOs and never opens /dev/uinput.
      boot.kernelModules = lib.optional cfg.probe.enable "uinput";
      systemd.services.lab-probe = lib.mkIf cfg.probe.enable {
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${pkgs.python3}/bin/python3 ${labProbe} --consumer-user ${cfg.probe.consumerUser}"
            + lib.optionalString (cfg.probe.username != "") " --username ${lib.escapeShellArg cfg.probe.username}"
            + lib.optionalString earlyKbd " --early-keyboard";
          SyslogIdentifier = "lab-probe";
        };
      };

      boot.initrd.systemd.services.lab-probe-kbd = lib.mkIf earlyKbd {
        description = "lab-probe uinput keyboard holder (stage 1 through switch-root)";
        wantedBy = [ "initrd.target" ];
        wants = [ "systemd-modules-load.service" ];
        after = [ "systemd-modules-load.service" ];
        before = [
          cfg.displayManagerUnit
          "systemd-udev-settle.service"
          "initrd-switch-root.service"
        ];
        unitConfig = {
          DefaultDependencies = false;
          IgnoreOnIsolate = true;
          # Keeps the holder alive through systemd's final kill at the pivot.
          SurviveFinalKillSignal = "yes";
        };
        serviceConfig = {
          Type = "notify";
          ExecStart = "${labKbd}/bin/lab-probe-kbd";
          SyslogIdentifier = "lab-probe-kbd";
          KillMode = "process";
        };
      };
      systemd.services.lab-probe-kbd =
        let
          initrdUnit = config.boot.initrd.systemd.services.lab-probe-kbd;
        in
        lib.mkIf earlyKbd {
          inherit (initrdUnit) description serviceConfig;
          unitConfig = {
            inherit (initrdUnit.unitConfig)
              DefaultDependencies IgnoreOnIsolate SurviveFinalKillSignal;
          };
          restartIfChanged = false;
        };

      # Lab image: for the listed services, U2F is forced off and blank passwords
      # are refused (allowNullPassword = false drops nullok from pam_unix's auth
      # lines). U2F is off so a host's setting cannot prompt for a token there;
      # blank passwords are refused because the lab's probe must not log in
      # through a passwordless account.
      security.pam.services = lib.genAttrs cfg.u2fOffServices (_: {
        u2f.enable = lib.mkForce false;
        allowNullPassword = lib.mkForce false;
      });
    })
    (lib.mkIf cfg.vkmsCrc.enable {
      # vkms CRC sampler (lab-vkms-crc). Gated on its own option, not on
      # halmasuit.lab.enable, so a VM or EC2 host runs it without the gnomon
      # rescue settings. It loads no module and keeps no console off vkms;
      # the caller does that. The stage-1 unit gets debugfs from its own mount.
      boot.initrd.systemd.storePaths = [ labVkmsCrc ];
      boot.initrd.systemd.mounts = [
        {
          what = "debugfs";
          where = "/sys/kernel/debug";
          type = "debugfs";
          unitConfig.DefaultDependencies = false;
        }
      ];
      boot.initrd.systemd.services.lab-vkms-crc-initrd = {
        wantedBy = [ "sysinit.target" ];
        requires = [ "sys-kernel-debug.mount" ];
        after = [ "sys-kernel-debug.mount" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          ExecStart = "${labVkmsCrc}/bin/lab-vkms-crc";
          SyslogIdentifier = "lab-vkms-crc";
          Restart = "on-failure";
        };
      };
      systemd.services.lab-vkms-crc = {
        wantedBy = [ "sysinit.target" ];
        before = [ cfg.displayManagerUnit ];
        requires = [ "sys-kernel-debug.mount" ];
        after = [ "sys-kernel-debug.mount" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          ExecStart = "${labVkmsCrc}/bin/lab-vkms-crc";
          SyslogIdentifier = "lab-vkms-crc";
          Restart = "on-failure";
        };
      };
    })
  ];
}
