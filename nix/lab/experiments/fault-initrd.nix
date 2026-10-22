# Stalls stage 1 before root mounts. The stall unit runs `sleep infinity` as a
# oneshot with no start timeout, so sysroot.mount never starts. The stage-1
# deadline then reboots the machine.
{ pkgs, ... }:
{
  halmasuit.lab.enable = true;

  boot.initrd.systemd.services.lab-fault-initrd = {
    wantedBy = [ "initrd.target" ];
    before = [ "sysroot.mount" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = "infinity";
      ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
    };
  };
}
