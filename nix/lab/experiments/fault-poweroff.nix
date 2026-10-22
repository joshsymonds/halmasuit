# Powers the machine off once graphical.target is reached. The poweroff is
# queued with --no-block so the unit does not wait on a shutdown that stops it.
{ config, ... }:
{
  halmasuit.lab.enable = true;

  systemd.services.lab-fault-poweroff = {
    wantedBy = [ "graphical.target" ];
    after = [ "graphical.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${config.systemd.package}/bin/systemctl --no-block poweroff";
    };
  };
}
