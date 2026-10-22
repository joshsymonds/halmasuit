# Pure-eval structural gate for the gnomon lab module (nix/lab/module.nix,
# exported as nixosModules.gnomon-lab) and its experiments (exported as
# lib.labExperiments). Evaluates the module against a gnomon-like synthetic
# system with NON-default option values, then asserts every rescue setting is
# present and wired to its option. Each experiment is composed with the module
# on the same host and must carry its own fault. Builds no system closure: the
# derivation only writes a marker file when every check passes.
{ system, nixpkgs, labModule, labExperiments, labPatches }:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  # Distinct from the module defaults (180, 300, enp7s0, display-manager.service, greeter)
  # so a setting that is silently dropped or wired to the wrong option fails.
  labOptions = {
    enable = true;
    displayManagerUnit = "lab-dm.service";
    stage1Deadline = 241;
    rootfsDeadline = 421;
    wolInterface = "enp9s0";
    crcSampler.enable = true;
    probe = {
      enable = true;
      consumerUser = "labuser";
    };
    netconsole = {
      enable = true;
      targetIp = "10.20.30.40";
      sourceIp = "10.20.30.50";
      interface = "enp10s0";
      port = 6700;
    };
    u2fOffServices = [ "login" "sudo" ];
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

  # initrd storePaths entries are packages or { source, target, ... } sets.
  inStorePaths = c: pkg:
    builtins.any (p: "${if p ? source then p.source else p}" == "${pkg}")
      (get c [ "boot" "initrd" "systemd" "storePaths" ] [ ]);

  # The CRC sampler, built from the same derivation the module stages.
  labCrc = pkgs.callPackage ../nix/lab/lab-crc.nix { };

  # Cadence harness: includes lab-crc.c unchanged and runs its real sampler
  # loop against scripted DRM and a simulated clock. The clock moves only when
  # the loop sleeps, to the sleep's deadline, so pacing is exact and no
  # scenario reads the host clock. Each scenario states the rounds that
  # re-read head activity and the rounds that sample each head.
  cadenceSource = pkgs.writeText "lab-crc-cadence.c" ''
    #include <stdarg.h>
    #define main lab_crc_main
    #include "${../nix/lab/lab-crc.c}"
    #undef main

    /*
     * A round is one pass of the sampler loop, ended by its pacing sleep.
     * Round k starts at (k - 1) * PACING_NS, so the run covers two
     * ACTIVITY_PERIOD_NS periods and the re-reads due at 1 s and 2 s fall
     * inside it.
     */
    #define CADENCE_ROUNDS 1100
    #define CADENCE_HEADS 2

    struct scenario {
    	const char *name;
    	/* Round from which each head reads active; 0 means never. */
    	uint32_t active_from[CADENCE_HEADS];
    	/* Activity is re-read in rounds 1 through dense_refresh. */
    	uint32_t dense_refresh;
    	/* Later rounds that re-read activity; unused entries are 0. */
    	uint32_t refresh_at[4];
    };

    static const struct scenario scenarios[] = {
    	/* No head is ever active: every round re-reads activity. */
    	{ "idle", { 0, 0 }, CADENCE_ROUNDS, { 0 } },
    	/*
    	 * Head 40 reads active in round 4, so round 4 re-reads and samples it.
    	 * The next re-reads come ACTIVITY_PERIOD_NS after round 4's: round 504
    	 * (1006 ms), then round 1004 (2006 ms).
    	 */
    	{ "wake", { 4, 0 }, 4, { 504, 1004 } },
    	/*
    	 * Head 40 is active from round 1: re-reads at round 1, then only at
    	 * round 501 (1000 ms) and round 1001 (2000 ms).
    	 */
    	{ "active", { 1, 0 }, 1, { 501, 1001 } },
    	/* Head 41 is active from round 1 and head 40 never is: one head suffices. */
    	{ "any", { 0, 1 }, 1, { 501, 1001 } },
    };

    static uint32_t crtc_ids[CADENCE_HEADS] = { 40, 41 };
    static const struct scenario *active_scenario;
    static struct timespec sim_now = { 100, 0 };
    static uint32_t rounds_done;
    static bool refreshed[CADENCE_ROUNDS + 1];
    static bool sampled[CADENCE_HEADS][CADENCE_ROUNDS + 1];

    static uint32_t current_round(void)
    {
    	return rounds_done + 1;
    }

    static size_t head_index(uint32_t crtc_id)
    {
    	for (size_t i = 0; i < CADENCE_HEADS; i++) {
    		if (crtc_ids[i] == crtc_id)
    			return i;
    	}
    	fprintf(stderr, "cadence: unknown crtc %" PRIu32 "\n", crtc_id);
    	exit(2);
    }

    static bool head_active(size_t index)
    {
    	uint32_t from = active_scenario->active_from[index];

    	return from != 0 && current_round() >= from;
    }

    static bool refresh_expected(uint32_t round)
    {
    	if (round <= active_scenario->dense_refresh)
    		return true;
    	for (size_t i = 0; i < sizeof active_scenario->refresh_at / sizeof active_scenario->refresh_at[0]; i++) {
    		if (active_scenario->refresh_at[i] == round)
    			return true;
    	}
    	return false;
    }

    static bool check_refresh(void)
    {
    	for (uint32_t r = 1; r <= CADENCE_ROUNDS; r++) {
    		bool want = refresh_expected(r);

    		if (refreshed[r] != want) {
    			fprintf(stderr, "FAIL %s: round %" PRIu32 " re-read activity=%d, expected %d\n",
    				active_scenario->name, r, refreshed[r], want);
    			return false;
    		}
    	}
    	return true;
    }

    static bool check_sampling(size_t head)
    {
    	uint32_t from = active_scenario->active_from[head];

    	for (uint32_t r = 1; r <= CADENCE_ROUNDS; r++) {
    		bool want = from != 0 && r >= from;

    		if (sampled[head][r] != want) {
    			fprintf(stderr, "FAIL %s: head %" PRIu32 " sampled=%d in round %" PRIu32 ", expected %d\n",
    				active_scenario->name, crtc_ids[head], sampled[head][r], r, want);
    			return false;
    		}
    	}
    	return true;
    }

    static _Noreturn void finish(void)
    {
    	bool ok = check_refresh();

    	for (size_t i = 0; i < CADENCE_HEADS; i++) {
    		if (!check_sampling(i))
    			ok = false;
    	}
    	if (ok)
    		printf("PASS %s\n", active_scenario->name);
    	exit(ok ? EXIT_SUCCESS : EXIT_FAILURE);
    }

    /* Referenced by lab_crc_main, which the harness never runs. */
    int drmIsMaster(int fd)
    {
    	(void)fd;
    	return 0;
    }

    int drmDropMaster(int fd)
    {
    	(void)fd;
    	return 0;
    }

    drmModeResPtr drmModeGetResources(int fd)
    {
    	drmModeResPtr res;

    	(void)fd;
    	res = calloc(1, sizeof *res);
    	if (res == NULL)
    		return NULL;
    	res->count_crtcs = CADENCE_HEADS;
    	res->crtcs = crtc_ids;
    	return res;
    }

    void drmModeFreeResources(drmModeResPtr ptr)
    {
    	free(ptr);
    }

    drmModeCrtcPtr drmModeGetCrtc(int fd, uint32_t crtcId)
    {
    	drmModeCrtcPtr crtc;

    	(void)fd;
    	crtc = calloc(1, sizeof *crtc);
    	if (crtc == NULL)
    		return NULL;
    	crtc->mode_valid = head_active(head_index(crtcId));
    	refreshed[current_round()] = true;
    	return crtc;
    }

    void drmModeFreeCrtc(drmModeCrtcPtr ptr)
    {
    	free(ptr);
    }

    int ioctl(int fd, unsigned long int request, ...)
    {
    	struct drm_nvidia_get_crtc_crc32_v2_params *p;
    	va_list ap;

    	(void)fd;
    	if (request != DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2) {
    		errno = ENOTTY;
    		return -1;
    	}
    	va_start(ap, request);
    	p = va_arg(ap, struct drm_nvidia_get_crtc_crc32_v2_params *);
    	va_end(ap);
    	sampled[head_index(p->crtc_id)][current_round()] = true;
    	p->comp.supported = 1;
    	p->rg.supported = 1;
    	p->out.supported = 1;
    	return 0;
    }

    int clock_gettime(clockid_t clock_id, struct timespec *ts)
    {
    	(void)clock_id;
    	*ts = sim_now;
    	return 0;
    }

    int clock_nanosleep(clockid_t clock_id, int flags,
    		    const struct timespec *request, struct timespec *remain)
    {
    	(void)clock_id;
    	(void)remain;
    	if (flags != TIMER_ABSTIME) {
    		fprintf(stderr, "cadence: relative sleep is not modelled\n");
    		exit(2);
    	}
    	sim_now = *request;
    	rounds_done++;
    	if (rounds_done == CADENCE_ROUNDS)
    		finish();
    	return 0;
    }

    int main(int argc, char **argv)
    {
    	if (argc != 2) {
    		fprintf(stderr, "usage: lab-crc-cadence SCENARIO\n");
    		return 2;
    	}
    	for (size_t i = 0; i < sizeof scenarios / sizeof scenarios[0]; i++) {
    		if (strcmp(scenarios[i].name, argv[1]) == 0)
    			active_scenario = &scenarios[i];
    	}
    	if (active_scenario == NULL) {
    		fprintf(stderr, "lab-crc-cadence: unknown scenario %s\n", argv[1]);
    		return 2;
    	}
    	sample_forever(-1, "card1");
    }
  '';

  cadenceScenarios = [ "idle" "wake" "active" "any" ];

  labCrcCadence = pkgs.stdenv.mkDerivation {
    pname = "lab-crc-cadence";
    version = "0";

    dontUnpack = true;
    dontConfigure = true;

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.libdrm ];

    buildPhase = ''
      runHook preBuild
      $CC -Wall -Wextra -Werror -O2 ${cadenceSource} $(pkg-config --cflags libdrm) -o lab-crc-cadence
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm755 lab-crc-cadence $out/bin/lab-crc-cadence
      runHook postInstall
    '';
  };

  # The keyboard probe script, staged by the module as the lab-probe unit's payload.
  labProbe = ../nix/lab/lab_probe.py;

  # ── Lab instrumentation fixtures ─────────────────────────────────────
  nc = labOptions.netconsole;
  netconsoleDevice = "sys-subsystem-net-devices-${nc.interface}.device";
  netconsoleUnit = get cfg [ "boot" "initrd" "systemd" "services" "lab-netconsole" ] { };

  # The same host with one netconsole field emptied; its assertions must fail.
  netconsoleWithEmpty = field:
    (evalWith (labOptions // { netconsole = nc // { ${field} = ""; }; })).config;

  # Every new setting non-default, with the lab disabled: nothing may appear.
  disabledNew = (evalWith (labOptions // { enable = false; })).config;
  # The same host with netconsole off: the firewall must match it exactly.
  netconsoleOff = (evalWith (labOptions // { netconsole = nc // { enable = false; }; })).config;
  # The same host with the CRC sampler off: neither unit may appear.
  crcOff = (evalWith (labOptions // { crcSampler = { enable = false; }; })).config;
  # The same host with the probe off: neither the unit nor uinput may appear.
  probeOff = (evalWith (labOptions // { probe = { enable = false; }; })).config;
  # The same host with a probe username: it must reach ExecStart as --username.
  probeUser = (evalWith (labOptions // { probe = labOptions.probe // { username = "labprobe"; }; })).config;
  # No lab settings at all: the new options must default off or empty, except
  # probe.consumerUser, which defaults to greeter.
  defaults = (evalWith { }).config.halmasuit.lab;

  # The host enables U2F globally and explicitly turns on one listed service,
  # so the lab override must beat both. Disabled, neither is forced off.
  u2fHost = lab: (lib.nixosSystem {
    inherit system;
    modules = [
      labModule
      (gnomonLike lab)
      {
        security.pam.u2f.enable = true;
        security.pam.services.sudo.u2f.enable = true;
      }
    ];
  }).config;
  u2fOn = u2fHost labOptions;
  u2fOff = u2fHost (labOptions // { enable = false; });

  failingAssertions = c: builtins.filter (a: !a.assertion) c.assertions;

  # Journal contract text frozen in nix/lab/module.nix for tests/hw/timeline.py.
  moduleSource = builtins.readFile labModule;
  journalContract = [
    "SYSLOG_IDENTIFIER=lab-crc"
    "lab-crc-initrd.service"
    "lab-crc.service"
    "start t=<us>"
    "sample card=<cardN> crtc=<id> t=<us> comp=<hex> rg=<hex> out=<hex> comp_sup=<0|1> rg_sup=<0|1> out_sup=<0|1>"
    "error card=<cardN> crtc=<id> t=<us> errno=<n>"
    "SYSLOG_IDENTIFIER=lab-probe"
    "lab-probe.service"
    "device node=<path>"
    "ready holder=<comm> pid=<pid>"
    "typed keys=<n>"
    "error reason=<text>"
    "8 lowercase digits"
  ];

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

    # ── Lab instrumentation options: declared, defaulted, carried ────
    { ok = (get cfg [ "halmasuit" "lab" "crcSampler" "enable" ] false) == true;
      msg = "halmasuit.lab.crcSampler.enable does not carry its value"; }
    { ok = (get cfg [ "halmasuit" "lab" "probe" "enable" ] false) == true;
      msg = "halmasuit.lab.probe.enable does not carry its value"; }
    { ok = (get cfg [ "halmasuit" "lab" "probe" "consumerUser" ] null) == labOptions.probe.consumerUser;
      msg = "halmasuit.lab.probe.consumerUser does not carry its value"; }
    { ok = !defaults.crcSampler.enable && !defaults.probe.enable && !defaults.netconsole.enable
        && defaults.netconsole.targetIp == "" && defaults.netconsole.sourceIp == ""
        && defaults.netconsole.interface == "" && defaults.netconsole.port == 6666
        && defaults.u2fOffServices == [ ];
      msg = "a new lab option does not default off or empty (netconsole.port defaults to 6666)"; }
    { ok = (get defaults [ "probe" "consumerUser" ] null) == "greeter";
      msg = "halmasuit.lab.probe.consumerUser does not default to \"greeter\""; }
    { ok = (get defaults [ "probe" "username" ] null) == "";
      msg = "halmasuit.lab.probe.username does not default to \"\""; }

    # ── Netconsole from stage 1: outbound UDP only ───────────────────
    { ok = builtins.elem "netconsole" (get cfg [ "boot" "initrd" "availableKernelModules" ] [ ]);
      msg = "netconsole is not in boot.initrd.availableKernelModules"; }
    { ok = inStorePaths cfg pkgs.kmod;
      msg = "kmod (modprobe) is not in boot.initrd.systemd.storePaths"; }
    { ok = (netconsoleUnit.serviceConfig.ExecStart or null)
        == "${pkgs.kmod}/bin/modprobe netconsole netconsole=+@${nc.sourceIp}/${nc.interface},${toString nc.port}@${nc.targetIp}/";
      msg = "lab-netconsole ExecStart is not modprobe netconsole with source, interface, port and target"; }
    { ok = (netconsoleUnit.serviceConfig.Type or null) == "oneshot"
        && (netconsoleUnit.serviceConfig.RemainAfterExit or false) == true;
      msg = "lab-netconsole is not a oneshot with RemainAfterExit=yes"; }
    { ok = builtins.elem netconsoleDevice (netconsoleUnit.bindsTo or [ ])
        && builtins.elem netconsoleDevice (netconsoleUnit.after or [ ]);
      msg = "lab-netconsole is not BindsTo= and After= ${netconsoleDevice}"; }
    { ok = builtins.elem "initrd.target" (netconsoleUnit.wantedBy or [ ])
        && (netconsoleUnit.unitConfig.DefaultDependencies or true) == false;
      msg = "lab-netconsole is not wanted by initrd.target with DefaultDependencies=no"; }
    { ok = builtins.elem "initrd.target" (netconsoleUnit.before or [ ]);
      msg = "lab-netconsole is not Before=initrd.target (stage 1 can switch root before modprobe finishes)"; }
    { ok = failingAssertions cfg == [ ];
      msg = "the valid lab config fails an assertion"; }
    { ok = lib.all (f: failingAssertions (netconsoleWithEmpty f) != [ ]) [ "targetIp" "sourceIp" "interface" ];
      msg = "netconsole.enable with an empty targetIp, sourceIp or interface passes its assertion"; }
    { ok = cfg.networking.firewall.allowedUDPPorts == netconsoleOff.networking.firewall.allowedUDPPorts
        && cfg.networking.firewall.allowedTCPPorts == netconsoleOff.networking.firewall.allowedTCPPorts
        && cfg.networking.firewall.allowedUDPPortRanges == netconsoleOff.networking.firewall.allowedUDPPortRanges
        && cfg.networking.firewall.allowedTCPPortRanges == netconsoleOff.networking.firewall.allowedTCPPortRanges
        && !(builtins.elem nc.port cfg.networking.firewall.allowedUDPPorts);
      msg = "netconsole changes the firewall (it must open no port)"; }
    { ok = (get disabledNew [ "boot" "initrd" "systemd" "services" "lab-netconsole" ] null) == null
        && !(builtins.elem "netconsole" (get disabledNew [ "boot" "initrd" "availableKernelModules" ] [ ]));
      msg = "enable = false still defines lab-netconsole or loads the netconsole module"; }

    # ── u2fOffServices: forced off, and only when the lab is enabled ─
    { ok = (get u2fOn [ "security" "pam" "u2f" "enable" ] false) == true;
      msg = "the U2F host fixture does not enable U2F (the override check would be vacuous)"; }
    { ok = lib.all (name: (get u2fOn [ "security" "pam" "services" name "u2f" "enable" ] null) == false)
        labOptions.u2fOffServices;
      msg = "u2fAuth is not false for every u2fOffServices entry on a U2F-enabled host"; }
    { ok = lib.all (name: (get u2fOff [ "security" "pam" "services" name "u2f" "enable" ] null) != false)
        labOptions.u2fOffServices;
      msg = "enable = false still forces u2fAuth off for u2fOffServices"; }

    # ── Patch export ─────────────────────────────────────────────────
    { ok = toString labPatches.nvidiaCrcVblankWait == toString ./lib/crc-vblank-wait.patch;
      msg = "lib.labPatches.nvidiaCrcVblankWait is not tests/lib/crc-vblank-wait.patch"; }
  ];

  # Journal contract documented in the module header for the lab-crc and
  # lab-probe producers. A source-text check: the contract is a comment, so
  # it has no evaluated form to assert on.
  journalChecks = map (s: {
    ok = lib.hasInfix s moduleSource;
    msg = "nix/lab/module.nix does not document journal contract text: ${s}";
  }) journalContract;

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

  # ── CRC sampler units: stage 1 and rootfs ────────────────────────────
  crcInitrd = get cfg [ "boot" "initrd" "systemd" "services" "lab-crc-initrd" ] { };
  crcRoot = get cfg [ "systemd" "services" "lab-crc" ] { };
  crcUnitChecks = label: unit: [
    { ok = (unit.serviceConfig.ExecStart or null) == "${labCrc}/bin/lab-crc";
      msg = "${label} does not run the lab-crc sampler binary"; }
    { ok = (unit.serviceConfig.SyslogIdentifier or null) == "lab-crc";
      msg = "${label} lacks SyslogIdentifier=lab-crc"; }
    { ok = (unit.serviceConfig.Restart or null) == "on-failure";
      msg = "${label} does not restart the sampler on failure"; }
    { ok = (unit.unitConfig.DefaultDependencies or true) == false;
      msg = "${label} lacks DefaultDependencies=no"; }
    { ok = builtins.elem "sysinit.target" (unit.wantedBy or [ ]);
      msg = "${label} is not wanted by sysinit.target"; }
  ];
  crcChecks = crcUnitChecks "lab-crc-initrd.service" crcInitrd
    ++ crcUnitChecks "lab-crc.service" crcRoot ++ [
    { ok = inStorePaths cfg labCrc;
      msg = "the lab-crc sampler is not in boot.initrd.systemd.storePaths (stage 1 would miss it)"; }
    { ok = builtins.elem labOptions.displayManagerUnit (crcRoot.before or [ ]);
      msg = "lab-crc.service is not Before=${labOptions.displayManagerUnit} (the display manager's DRM master must follow the drop)"; }
    { ok = (get crcOff [ "boot" "initrd" "systemd" "services" "lab-crc-initrd" ] null) == null
        && (get crcOff [ "systemd" "services" "lab-crc" ] null) == null
        && !(inStorePaths crcOff labCrc);
      msg = "crcSampler.enable = false still defines a lab-crc unit or stages the sampler"; }
    { ok = (get disabledNew [ "boot" "initrd" "systemd" "services" "lab-crc-initrd" ] null) == null
        && (get disabledNew [ "systemd" "services" "lab-crc" ] null) == null
        && !(inStorePaths disabledNew labCrc);
      msg = "enable = false (with crcSampler.enable = true) still defines a lab-crc unit or stages the sampler"; }
  ];

  # ── lab-probe: on-demand root oneshot, never started on its own ──────
  probeUnit = get cfg [ "systemd" "services" "lab-probe" ] { };
  probeChecks = [
    { ok = (probeUnit.serviceConfig.Type or null) == "oneshot";
      msg = "lab-probe.service is missing or not a oneshot"; }
    { ok = (probeUnit.serviceConfig.ExecStart or null) == "${pkgs.python3}/bin/python3 ${labProbe} --consumer-user ${labOptions.probe.consumerUser}";
      msg = "lab-probe.service does not run lab_probe.py --consumer-user with python3"; }
    { ok = (get probeUser [ "systemd" "services" "lab-probe" "serviceConfig" "ExecStart" ] null) == "${pkgs.python3}/bin/python3 ${labProbe} --consumer-user ${labOptions.probe.consumerUser} --username labprobe";
      msg = "lab-probe.service does not pass a set probe.username as --username"; }
    { ok = !lib.hasInfix "--username" (probeUnit.serviceConfig.ExecStart or "");
      msg = "lab-probe.service passes --username while probe.username is empty"; }
    { ok = (probeUnit.serviceConfig.SyslogIdentifier or null) == "lab-probe";
      msg = "lab-probe.service lacks SyslogIdentifier=lab-probe"; }
    { ok = lib.all (key: (probeUnit.${key} or [ ]) == [ ]) [ "wantedBy" "requiredBy" "upheldBy" ];
      msg = "lab-probe.service has a wantedBy, requiredBy or upheldBy entry (it must never start on its own)"; }
    { ok = lib.all (kind: !(builtins.hasAttr "lab-probe" (get cfg [ "systemd" kind ] { })))
        [ "timers" "paths" "sockets" ];
      msg = "a timer, path or socket unit named lab-probe can activate the probe"; }
    { ok = builtins.elem "uinput" (get cfg [ "boot" "kernelModules" ] [ ]);
      msg = "uinput is not in boot.kernelModules"; }
    { ok = (get probeOff [ "systemd" "services" "lab-probe" ] null) == null
        && !(builtins.elem "uinput" (get probeOff [ "boot" "kernelModules" ] [ ]));
      msg = "probe.enable = false still defines lab-probe or loads uinput"; }
    { ok = (get disabledNew [ "systemd" "services" "lab-probe" ] null) == null
        && !(builtins.elem "uinput" (get disabledNew [ "boot" "kernelModules" ] [ ]));
      msg = "enable = false (with probe.enable = true) still defines lab-probe or loads uinput"; }
  ];

  allChecks = checks ++ journalChecks ++ experimentChecks ++ crcChecks ++ probeChecks;
  failures = builtins.filter (c: !c.ok) allChecks;
  report = lib.concatMapStrings
    (c: "echo ${lib.escapeShellArg "FAIL: ${c.msg}"} >&2\n")
    failures;
in
pkgs.runCommand "gnomon-lab-eval" { } ''
  ${report}
  ${lib.optionalString (failures != [ ]) "exit 1"}
  test -x ${labCrc}/bin/lab-crc
  ${lib.concatMapStrings (s: "${labCrcCadence}/bin/lab-crc-cadence ${s}\n") cadenceScenarios}
  echo "gnomon-lab-eval: ${toString (builtins.length allChecks)} checks passed, ${toString (builtins.length cadenceScenarios)} lab-crc cadence scenarios passed" > $out
''
