# Keeps displayManagerUnit from ever becoming active after root. The fault unit
# is ordered before it and runs `sleep infinity` with no start timeout. It never
# defines a unit under that name: greetd provides display-manager.service as an
# alias of greetd.service, and a second definition would conflict with it.
{ config, pkgs, ... }:
{
  halmasuit.lab.enable = true;

  systemd.services.lab-fault-hang = {
    wantedBy = [ "graphical.target" ];
    before = [ config.halmasuit.lab.displayManagerUnit ];
    serviceConfig = {
      Type = "oneshot";
      TimeoutStartSec = "infinity";
      ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
    };
  };
}
