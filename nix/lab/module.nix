# Gnomon lab rescue module. Carries every setting that lets an experimental
# boot on gnomon recover without a physical visit: a hardware watchdog armed
# in both boot stages, a stage-1 deadline, a rootfs deadline, reboot on
# kernel panic, and Wake-on-LAN. Everything is gated on halmasuit.lab.enable.
# The module declares no specialisation and adds no generation or boot entry.
#
# Lab instrumentation settings: netconsole from the initramfs, U2F off for
# listed PAM services, and the crcSampler and probe options whose units are
# not defined here yet. Every option defaults off or empty.
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

    probe.enable = lib.mkEnableOption "the lab-probe uinput keyboard and probe unit";

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

  config = lib.mkIf cfg.enable {
    # Hardware watchdog: stage 1 arms it, and the rootfs keeps it armed.
    boot.initrd.kernelModules = [ "sp5100_tco" ];
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
    # exists and stays loaded across switch-root. It sends UDP only, and
    # the firewall is not touched.
    assertions = [
      {
        assertion = !cfg.netconsole.enable
          || (cfg.netconsole.targetIp != "" && cfg.netconsole.sourceIp != "" && cfg.netconsole.interface != "");
        message = "halmasuit.lab.netconsole needs targetIp, sourceIp and interface when enabled.";
      }
    ];
    boot.initrd.availableKernelModules = lib.optional cfg.netconsole.enable "netconsole";
    boot.initrd.systemd.storePaths = lib.optional cfg.netconsole.enable pkgs.kmod;
    boot.initrd.systemd.services.lab-netconsole = lib.mkIf cfg.netconsole.enable {
      wantedBy = [ "initrd.target" ];
      bindsTo = [ netconsoleDevice ];
      after = [ netconsoleDevice ];
      unitConfig.DefaultDependencies = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.kmod}/bin/modprobe netconsole netconsole=+@${cfg.netconsole.sourceIp}/${cfg.netconsole.interface},${toString cfg.netconsole.port}@${cfg.netconsole.targetIp}/";
      };
    };

    # Lab image: U2F off for the listed services, overriding the host's setting.
    security.pam.services = lib.genAttrs cfg.u2fOffServices (_: {
      u2f.enable = lib.mkForce false;
    });
  };
}
