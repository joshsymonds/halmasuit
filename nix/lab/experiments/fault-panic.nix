# Panics the kernel once graphical.target is reached. The unit syncs first, then
# writes `c` to sysrq-trigger. The module's panic=10 reboots the machine ten
# seconds after the panic.
{ pkgs, ... }:
{
  halmasuit.lab.enable = true;

  systemd.services.lab-fault-panic = {
    wantedBy = [ "graphical.target" ];
    after = [ "graphical.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStartPre = "${pkgs.coreutils}/bin/sync";
      ExecStart = "${pkgs.coreutils}/bin/echo c";
      StandardOutput = "file:/proc/sysrq-trigger";
    };
  };
}
