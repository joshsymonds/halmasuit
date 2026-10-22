# Gnomon lab rescue module. Carries every setting that lets an experimental
# boot on gnomon recover without a physical visit: a hardware watchdog armed
# in both boot stages, a stage-1 deadline, a rootfs deadline, reboot on
# kernel panic, and Wake-on-LAN. Everything is gated on halmasuit.lab.enable.
# The module declares no specialisation and adds no generation or boot entry.
{ config, lib, ... }:

let
  cfg = config.halmasuit.lab;

  initrdSystemctl = "${config.boot.initrd.systemd.package}/bin/systemctl";
  systemctl = "${config.systemd.package}/bin/systemctl";

  watchdogManager = {
    WatchdogDevice = "/dev/watchdog0";
    RuntimeWatchdogSec = "60s";
  };
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
  };
}
