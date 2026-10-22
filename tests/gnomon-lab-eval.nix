# Pure-eval structural gate for the gnomon lab module (nix/lab/module.nix,
# exported as nixosModules.gnomon-lab) and its experiments (exported as
# lib.labExperiments). Evaluates the module against a gnomon-like synthetic
# system with NON-default option values, then asserts every rescue setting is
# present and wired to its option. Each experiment is composed with the module
# on the same host and must carry its own fault. Builds no system closure: the
# derivation only writes a marker file when every check passes.
{ system, nixpkgs, labModule, labExperiments }:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  # Distinct from the module defaults (180, 300, enp7s0, display-manager.service)
  # so a setting that is silently dropped or wired to the wrong option fails.
  labOptions = {
    enable = true;
    displayManagerUnit = "lab-dm.service";
    stage1Deadline = 241;
    rootfsDeadline = 421;
    wolInterface = "enp9s0";
  };

  # Synthetic gnomon-like host: systemd stage 1, LUKS root `cryptroot`
  # unlocked through FIDO2/TPM2 with `fido2-device=auto`. The stubs exist
  # only to satisfy NixOS's required-option assertions.
  gnomonLike = lab: { ... }: {
    boot.initrd.systemd.enable = true;
    boot.initrd.luks.devices.cryptroot = {
      device = "/dev/disk/by-partlabel/cryptroot";
      crypttabExtraOpts = [ "fido2-device=auto" ];
    };
    halmasuit.lab = lab;
    fileSystems."/" = { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
    boot.loader.systemd-boot.enable = true;
    system.stateVersion = "26.05";
  };

  evalWith = lab: lib.nixosSystem {
    inherit system;
    modules = [ labModule (gnomonLike lab) ];
  };

  cfg = (evalWith labOptions).config;
  disabled = (evalWith { enable = false; }).config;

  # Attribute lookup that yields the default instead of an eval error, so a
  # missing setting reports as a named FAIL line rather than an attr error.
  get = cfg': path: default: lib.attrByPath path default cfg';

  initrdManager = get cfg [ "boot" "initrd" "systemd" "settings" "Manager" ] { };
  rootManager = get cfg [ "systemd" "settings" "Manager" ] { };
  stage1Timer = get cfg [ "boot" "initrd" "systemd" "timers" "lab-stage1-deadline" ] { };
  stage1Service = get cfg [ "boot" "initrd" "systemd" "services" "lab-stage1-deadline" ] { };
  rootTimer = get cfg [ "systemd" "timers" "lab-rootfs-deadline" ] { };
  rootService = get cfg [ "systemd" "services" "lab-rootfs-deadline" ] { };
  rootReboot = get cfg [ "systemd" "services" "lab-rootfs-reboot" ] { };
  crypttabOpts = get cfg [ "boot" "initrd" "luks" "devices" "cryptroot" "crypttabExtraOpts" ] [ ];
  wolLink = get cfg [ "systemd" "network" "links" "40-${labOptions.wolInterface}" ] { };

  # The only reboot command the deadlines may run. One --force kills
  # processes and unmounts; a second --force skips unmounting entirely.
  initrdRebootCmd = "${cfg.boot.initrd.systemd.package}/bin/systemctl --force reboot";
  rootRebootCmd = "${cfg.systemd.package}/bin/systemctl --force reboot";

  armed = v: v != null && v != "0";

  checks = [
    # ── Hardware watchdog, stage 1 ───────────────────────────────────
    { ok = (initrdManager.WatchdogDevice or null) == "/dev/watchdog0";
      msg = "boot.initrd.systemd.settings.Manager.WatchdogDevice is not /dev/watchdog0"; }
    { ok = armed (initrdManager.RuntimeWatchdogSec or null);
      msg = "initrd watchdog is not armed (RuntimeWatchdogSec unset or 0)"; }
    { ok = builtins.elem "sp5100_tco" (get cfg [ "boot" "initrd" "kernelModules" ] [ ]);
      msg = "sp5100_tco is not in boot.initrd.kernelModules"; }

    # ── Hardware watchdog, rootfs ────────────────────────────────────
    { ok = (rootManager.WatchdogDevice or null) == "/dev/watchdog0";
      msg = "systemd.settings.Manager.WatchdogDevice (rootfs) is not /dev/watchdog0"; }
    { ok = armed (rootManager.RuntimeWatchdogSec or null);
      msg = "rootfs watchdog is not armed (RuntimeWatchdogSec unset or 0)"; }

    # ── Stage-1 deadline: force reboot unless root is mounted ────────
    { ok = (stage1Timer.timerConfig.OnBootSec or null) == "241s";
      msg = "stage-1 deadline timer OnBootSec is not stage1Deadline (241s)"; }
    # Disarm on initrd-switch-root.target, which initrd-cleanup.service
    # isolates only after root is mounted. sysroot.mount must not be the
    # disarm unit: its start is required by the initial initrd transaction,
    # so a Conflicts= on it deletes the timer's start job there.
    { ok = (stage1Timer.conflicts or [ ]) == [ "initrd-switch-root.target" ];
      msg = "stage-1 deadline timer is not Conflicts=initrd-switch-root.target (disarm only after root is mounted)"; }
    { ok = (stage1Timer.unitConfig.IgnoreOnIsolate or false) == true;
      msg = "stage-1 deadline timer lacks IgnoreOnIsolate (isolate would cancel it)"; }
    { ok = (stage1Timer.unitConfig.DefaultDependencies or true) == false;
      msg = "stage-1 deadline timer lacks DefaultDependencies=no (sysinit.target's Conflicts=emergency.target would stop it)"; }
    { ok = (stage1Service.unitConfig.IgnoreOnIsolate or false) == true;
      msg = "stage-1 deadline service lacks IgnoreOnIsolate (isolate would cancel it)"; }
    { ok = (stage1Service.unitConfig.DefaultDependencies or true) == false;
      msg = "stage-1 deadline service lacks DefaultDependencies=no (sysinit.target's Conflicts=emergency.target would stop it)"; }
    { ok = (stage1Service.serviceConfig.ExecStart or null) == initrdRebootCmd;
      msg = "stage-1 deadline service ExecStart is not `systemctl --force reboot` (one --force; a second skips unmounting)"; }

    # ── Rootfs deadline: reboot unless displayManagerUnit is active ──
    { ok = (rootTimer.timerConfig.OnBootSec or null) == "421s";
      msg = "rootfs deadline timer OnBootSec is not rootfsDeadline (421s)"; }
    { ok = (rootTimer.unitConfig.IgnoreOnIsolate or false) == true;
      msg = "rootfs deadline timer lacks IgnoreOnIsolate (isolate would cancel it)"; }
    { ok = (rootTimer.unitConfig.DefaultDependencies or true) == false;
      msg = "rootfs deadline timer lacks DefaultDependencies=no (sysinit.target's Conflicts=emergency.target would stop it)"; }
    { ok = (rootService.unitConfig.IgnoreOnIsolate or false) == true;
      msg = "rootfs deadline service lacks IgnoreOnIsolate (isolate would cancel it)"; }
    { ok = (rootService.unitConfig.DefaultDependencies or true) == false;
      msg = "rootfs deadline service lacks DefaultDependencies=no (sysinit.target's Conflicts=emergency.target would stop it)"; }
    { ok = lib.hasInfix "is-active --quiet ${labOptions.displayManagerUnit}" (rootService.serviceConfig.ExecStart or "");
      msg = "rootfs deadline service does not test displayManagerUnit (lab-dm.service) with is-active"; }
    { ok = builtins.elem "lab-rootfs-reboot.service" (rootService.onFailure or [ ]);
      msg = "rootfs deadline service does not OnFailure= the reboot unit"; }
    { ok = (rootReboot.serviceConfig.ExecStart or null) == rootRebootCmd;
      msg = "rootfs reboot unit ExecStart is not `systemctl --force reboot` (one --force; a second skips unmounting)"; }
    { ok = (rootReboot.unitConfig.IgnoreOnIsolate or false) == true;
      msg = "rootfs reboot unit lacks IgnoreOnIsolate (isolate would cancel it)"; }
    { ok = (rootReboot.unitConfig.DefaultDependencies or true) == false;
      msg = "rootfs reboot unit lacks DefaultDependencies=no (sysinit.target's Conflicts=emergency.target would stop it)"; }

    # ── LUKS unlock timing; fido2-device=auto must survive ───────────
    { ok = builtins.elem "fido2-device=auto" crypttabOpts;
      msg = "boot.initrd.luks.devices.cryptroot.crypttabExtraOpts lost fido2-device=auto"; }
    { ok = builtins.elem "timeout=120s" crypttabOpts;
      msg = "cryptroot crypttab timeout= is not stage1Deadline/2 (120s)"; }
    { ok = builtins.elem "token-timeout=30s" crypttabOpts;
      msg = "cryptroot crypttab token-timeout= is missing"; }

    # ── Panic reboot via kernel command line ─────────────────────────
    { ok = builtins.elem "panic=10" (get cfg [ "boot" "kernelParams" ] [ ]);
      msg = "panic=10 is not on the kernel command line"; }

    # ── Wake-on-LAN magic on wolInterface ────────────────────────────
    { ok = (wolLink.linkConfig.WakeOnLan or null) == "magic";
      msg = "WakeOnLan is not magic on the wolInterface link (40-enp9s0)"; }

    # ── No boot entry or generation from the module ──────────────────
    { ok = (get cfg [ "specialisation" ] { }) == { };
      msg = "module declares a specialisation (a boot entry)"; }

    # ── enable = false: the module adds nothing ──────────────────────
    { ok = (get disabled [ "boot" "initrd" "systemd" "timers" "lab-stage1-deadline" ] null) == null;
      msg = "enable = false still defines the stage-1 deadline timer"; }
    { ok = (get disabled [ "systemd" "settings" "Manager" "WatchdogDevice" ] null) != "/dev/watchdog0";
      msg = "enable = false still arms the rootfs watchdog"; }
    { ok = !(builtins.elem "panic=10" (get disabled [ "boot" "kernelParams" ] [ ]));
      msg = "enable = false still sets panic=10"; }
    { ok = (get disabled [ "systemd" "network" "links" "40-enp7s0" ] null) == null;
      msg = "enable = false still defines a WakeOnLan link"; }
  ];

  # ── Lab experiments, composed with the module on the same host ───────
  # Each experiment is evaluated the way nix-config composes it: the module,
  # the experiment, and the gnomon-like host with the non-default lab options.
  # The host leaves halmasuit.lab.enable unset, as gnomon's own config does, so
  # only the experiment can turn the rescue settings on.
  evalExperiment = module: (lib.nixosSystem {
    inherit system;
    modules = [ labModule module (gnomonLike (removeAttrs labOptions [ "enable" ])) ];
  }).config;

  # Names of the units each stage defines. An experiment may add its fault
  # unit and nothing else, so baseline must match the module alone.
  unitNames = c: {
    system = lib.attrNames (get c [ "systemd" "services" ] { });
    systemTimers = lib.attrNames (get c [ "systemd" "timers" ] { });
    initrd = lib.attrNames (get c [ "boot" "initrd" "systemd" "services" ] { });
    initrdTimers = lib.attrNames (get c [ "boot" "initrd" "systemd" "timers" ] { });
  };

  # Every rescue setting, asserted on the composed experiment. The toplevel
  # drvPath is forced with its string context discarded: the evaluation runs,
  # and nothing is built.
  rescueChecks = name: c:
    let
      initrdManager = get c [ "boot" "initrd" "systemd" "settings" "Manager" ] { };
      rootManager = get c [ "systemd" "settings" "Manager" ] { };
      stage1Timer = get c [ "boot" "initrd" "systemd" "timers" "lab-stage1-deadline" ] { };
      stage1Service = get c [ "boot" "initrd" "systemd" "services" "lab-stage1-deadline" ] { };
      rootTimer = get c [ "systemd" "timers" "lab-rootfs-deadline" ] { };
      rootService = get c [ "systemd" "services" "lab-rootfs-deadline" ] { };
      rootRebootUnit = get c [ "systemd" "services" "lab-rootfs-reboot" ] { };
      wolLink = get c [ "systemd" "network" "links" "40-${labOptions.wolInterface}" ] { };
      stage1RebootCmd = "${c.boot.initrd.systemd.package}/bin/systemctl --force reboot";
      rootRebootCmd = "${c.systemd.package}/bin/systemctl --force reboot";
    in [
      { ok = (get c [ "halmasuit" "lab" "enable" ] false) == true;
        msg = "${name}: halmasuit.lab.enable is not true"; }
      { ok = builtins.seq (builtins.unsafeDiscardStringContext c.system.build.toplevel.drvPath) true;
        msg = "${name}: system.build.toplevel.drvPath does not evaluate"; }

      { ok = armed (initrdManager.RuntimeWatchdogSec or null)
          && (initrdManager.WatchdogDevice or null) == "/dev/watchdog0";
        msg = "${name}: stage-1 watchdog is not armed on /dev/watchdog0"; }
      { ok = armed (rootManager.RuntimeWatchdogSec or null)
          && (rootManager.WatchdogDevice or null) == "/dev/watchdog0";
        msg = "${name}: rootfs watchdog is not armed on /dev/watchdog0"; }

      { ok = (stage1Timer.timerConfig.OnBootSec or null) == "${toString labOptions.stage1Deadline}s";
        msg = "${name}: stage-1 deadline timer is missing or has the wrong OnBootSec"; }
      { ok = (stage1Service.serviceConfig.ExecStart or null) == stage1RebootCmd;
        msg = "${name}: stage-1 deadline service does not reboot with one --force"; }

      { ok = (rootTimer.timerConfig.OnBootSec or null) == "${toString labOptions.rootfsDeadline}s";
        msg = "${name}: rootfs deadline timer is missing or has the wrong OnBootSec"; }
      { ok = lib.hasInfix "is-active --quiet ${labOptions.displayManagerUnit}" (rootService.serviceConfig.ExecStart or "");
        msg = "${name}: rootfs deadline service does not test ${labOptions.displayManagerUnit}"; }
      { ok = builtins.elem "lab-rootfs-reboot.service" (rootService.onFailure or [ ]);
        msg = "${name}: rootfs deadline service does not OnFailure= the reboot unit"; }
      { ok = (rootRebootUnit.serviceConfig.ExecStart or null) == rootRebootCmd;
        msg = "${name}: rootfs reboot unit does not reboot with one --force"; }

      { ok = builtins.elem "panic=10" (get c [ "boot" "kernelParams" ] [ ]);
        msg = "${name}: panic=10 is not on the kernel command line"; }
      { ok = (wolLink.linkConfig.WakeOnLan or null) == "magic";
        msg = "${name}: Wake-on-LAN magic is not enabled on ${labOptions.wolInterface}"; }
    ];

  # Each experiment's own fault, keyed by experiment name. The fault units are
  # asserted on the structure that makes them block or fire in the boot path.
  faultChecks = {
    baseline = name: c: [
      { ok = unitNames c == unitNames cfg;
        msg = "${name}: defines units beyond the module's"; }
    ];

    fault-initrd = name: c:
      let stall = get c [ "boot" "initrd" "systemd" "services" "lab-fault-initrd" ] { }; in [
        { ok = lib.hasSuffix "/bin/sleep infinity" (stall.serviceConfig.ExecStart or "");
          msg = "${name}: initrd stall unit does not run `sleep infinity`"; }
        { ok = (stall.serviceConfig.Type or null) == "oneshot"
            && (stall.serviceConfig.TimeoutStartSec or null) == "infinity";
          msg = "${name}: initrd stall unit is not a oneshot with TimeoutStartSec=infinity (a timeout lets root mount)"; }
        { ok = builtins.elem "sysroot.mount" (stall.before or [ ]);
          msg = "${name}: initrd stall unit is not Before=sysroot.mount"; }
        { ok = builtins.elem "initrd.target" (stall.wantedBy or [ ]);
          msg = "${name}: initrd stall unit is not wanted by initrd.target (outside the initial transaction)"; }
        { ok = (stall.unitConfig.DefaultDependencies or true) == false;
          msg = "${name}: initrd stall unit lacks DefaultDependencies=no"; }
      ];

    fault-hang = name: c:
      let
        hang = get c [ "systemd" "services" "lab-fault-hang" ] { };
        dm = labOptions.displayManagerUnit;
        services = get c [ "systemd" "services" ] { };
        units = get c [ "systemd" "units" ] { };
      in [
        { ok = lib.hasSuffix "/bin/sleep infinity" (hang.serviceConfig.ExecStart or "");
          msg = "${name}: hang unit does not run `sleep infinity`"; }
        { ok = (hang.serviceConfig.Type or null) == "oneshot"
            && (hang.serviceConfig.TimeoutStartSec or null) == "infinity";
          msg = "${name}: hang unit is not a oneshot with TimeoutStartSec=infinity (a timeout lets ${dm} start)"; }
        { ok = builtins.elem dm (hang.before or [ ]);
          msg = "${name}: hang unit is not Before=${dm}"; }
        { ok = builtins.elem "graphical.target" (hang.wantedBy or [ ]);
          msg = "${name}: hang unit is not wanted by graphical.target (the target that pulls the display manager)"; }
        { ok = !(builtins.hasAttr (lib.removeSuffix ".service" dm) services)
            && !(builtins.hasAttr dm units);
          msg = "${name}: defines a unit named ${dm}; greetd's alias would conflict with it"; }
      ];

    fault-panic = name: c:
      let panic = get c [ "systemd" "services" "lab-fault-panic" ] { }; in [
        { ok = lib.hasSuffix "/bin/sync" (panic.serviceConfig.ExecStartPre or "");
          msg = "${name}: fault unit does not run sync before the crash"; }
        { ok = lib.hasSuffix "/bin/echo c" (panic.serviceConfig.ExecStart or "");
          msg = "${name}: fault unit ExecStart is not `echo c`"; }
        { ok = (panic.serviceConfig.StandardOutput or null) == "file:/proc/sysrq-trigger";
          msg = "${name}: fault unit does not write to /proc/sysrq-trigger"; }
        { ok = builtins.elem "graphical.target" (panic.after or [ ])
            && builtins.elem "graphical.target" (panic.wantedBy or [ ]);
          msg = "${name}: fault unit does not run after graphical.target (after boot)"; }
      ];

    fault-poweroff = name: c:
      let off = get c [ "systemd" "services" "lab-fault-poweroff" ] { }; in [
        { ok = lib.hasSuffix "/bin/systemctl --no-block poweroff" (off.serviceConfig.ExecStart or "");
          msg = "${name}: fault unit does not power off with systemctl --no-block"; }
        { ok = builtins.elem "graphical.target" (off.after or [ ])
            && builtins.elem "graphical.target" (off.wantedBy or [ ]);
          msg = "${name}: fault unit does not run after graphical.target (after boot)"; }
      ];
  };

  experimentChecks = lib.concatLists (lib.mapAttrsToList
    (name: faultCheck:
      let c = evalExperiment labExperiments.${name}; in
      rescueChecks name c ++ faultCheck name c)
    faultChecks);

  allChecks = checks ++ experimentChecks;
  failures = builtins.filter (c: !c.ok) allChecks;
  report = lib.concatMapStrings
    (c: "echo ${lib.escapeShellArg "FAIL: ${c.msg}"} >&2\n")
    failures;
in
pkgs.runCommand "gnomon-lab-eval" { } ''
  ${report}
  ${lib.optionalString (failures != [ ]) "exit 1"}
  echo "gnomon-lab-eval: ${toString (builtins.length allChecks)} checks passed" > $out
''
