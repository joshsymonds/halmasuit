# Phase B: initramfs survival gate for the production halmasuit binary.
#
# Boots a NixOS VM with `services.halmasuit.fromInitrd.enable = true`,
# which registers halmasuit as a `boot.initrd.systemd.services.halmasuit`
# unit with `SurviveFinalKillSignal=yes`. Asserts:
#
#   1. systemd accepted SurviveFinalKillSignal=yes (no "Unknown key"
#      warning; misplacement in [Service] instead of [Unit] is silently
#      dropped, which would produce a passing-looking test that's
#      actually validating the wrong mechanism).
#   2. halmasuit started in initramfs, recorded its PID via the
#      `Started` NDJSON event, and emitted
#      `PhaseEntered { phase: "initramfs_init" }`.
#   3. The PID is still alive post-pivot (`kill -0` succeeds in rootfs).
#   4. The same process holds DRM master post-pivot
#      (`/sys/kernel/debug/dri/0/clients` reports it).
#   5. The NDJSON event stream spans both phases — the SAME PID emitted
#      `phase: "initramfs_init"` (pre-pivot) AND
#      `phase: "rootfs_ready"` (post-pivot), proving the pivot-poll
#      calloop source fired.
#   6. The Wayland socket is bound at /run/halmasuit/wayland-0 and
#      survived the pivot.
#
# This is the Phase 2 mechanism (RESEARCH.md L86-87) wrapped around the
# real halmasuit binary — drm-master-probe-phase2.nix proved the
# mechanic on a minimal probe; this proves halmasuit-the-whole-binary
# behaves the same way.
#
# Out of scope here (later Phase B tasks):
# - Greeter / halmasuit-luks foreground client
# - Frame-capture continuity (full-boot-flash.nix)
# - Post-pivot privilege drop

{
  system,
  nixpkgs,
  halmasuit,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "initrd-survival";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
      };

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import json
    import time

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Give the pivot-poll timer at least one cycle to fire post-rootfs.
    # halmasuit's calloop timer is 1s; 5s here is generous headroom.
    time.sleep(5)

    # ASSERTION 1: SurviveFinalKillSignal=yes was parsed by systemd, not
    # silently dropped as an "Unknown key". A misplaced directive
    # (serviceConfig instead of unitConfig) would produce this warning;
    # any apparent survival under that condition is via OTHER mechanisms
    # and not evidence for the hypothesis under test.
    parse_warn = machine.execute(
        "journalctl -b | grep -i 'Unknown key.*SurviveFinalKillSignal' || true"
    )[1].strip()
    if parse_warn:
        raise AssertionError(
            "systemd rejected SurviveFinalKillSignal as 'Unknown key' — "
            "directive lives in [Unit] (unitConfig), not [Service] "
            "(serviceConfig). nix/module.nix needs to place it correctly.\n"
            f"journal: {parse_warn}"
        )
    print("PASS: systemd parsed SurviveFinalKillSignal=yes")

    # Pull halmasuit's PID from the `Started` event. The unit lived in
    # initramfs systemd, so by the time rootfs is up the unit is no
    # longer registered with rootfs systemd — `journalctl -u` won't
    # find it. But the JSON log lines persist in journald (cross-pivot
    # journald continuity is the whole point of structured logging).
    # Filter by SYSLOG_IDENTIFIER or via `-o cat` + manual grep.
    raw = machine.succeed("journalctl -b --output=cat --no-pager")
    started_pid = None
    initramfs_init_seen = False
    rootfs_ready_seen = False
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            outer = json.loads(line)
        except Exception:
            continue
        if outer.get("target") != "halmasuit::event":
            continue
        inner_str = outer.get("fields", {}).get("json")
        if not inner_str:
            continue
        try:
            inner = json.loads(inner_str)
        except Exception:
            continue
        if inner.get("event") == "started":
            started_pid = inner.get("pid")
        elif inner.get("event") == "phase_entered":
            if inner.get("phase") == "initramfs_init":
                initramfs_init_seen = True
            elif inner.get("phase") == "rootfs_ready":
                rootfs_ready_seen = True

    if started_pid is None:
        raise AssertionError(
            "halmasuit did NOT emit a `Started` event — the unit may not "
            "have launched in initramfs.\n"
            "Tail of journal:\n"
            + machine.succeed("journalctl -b --output=cat --no-pager | tail -50")
        )
    print(f"halmasuit PID = {started_pid}")

    # ASSERTION 2: InitramfsInit was emitted (proves the
    # `is_initramfs()` branch fired).
    if not initramfs_init_seen:
        raise AssertionError(
            "halmasuit did NOT emit `phase: initramfs_init` — the "
            "runtime-detection branch in main.rs may not have fired. "
            "Check that /etc/initrd-release was present when halmasuit "
            "started.\n"
            "Phase events in journal:\n"
            + machine.execute("journalctl -b --output=cat --no-pager | grep -F 'phase_entered' || echo 'no phase_entered events found'")[1]
        )
    print("PASS: phase=initramfs_init emitted")

    # ASSERTION 3: PID survived the pivot. `kill -0` returns 0 iff the
    # process exists and signaling is permitted.
    alive = machine.execute(f"kill -0 {started_pid}")[0]
    if alive != 0:
        raise AssertionError(
            f"halmasuit PID {started_pid} is no longer alive in rootfs — "
            "SurviveFinalKillSignal=yes did NOT survive switch_root.\n"
            "Journal tail:\n"
            + machine.succeed("journalctl -b --output=cat --no-pager | tail -30")
        )
    print(f"PASS: PID {started_pid} survived switch_root")

    # ASSERTION 4: DRM master is still held by halmasuit. drm-master-
    # probe-phase2 uses the same /sys/kernel/debug/dri/0/clients
    # introspection (fourth field is master flag).
    drm_clients = machine.succeed("cat /sys/kernel/debug/dri/0/clients")
    found_master = False
    for line in drm_clients.splitlines():
        fields = line.split()
        if len(fields) >= 4 and fields[1] == str(started_pid) and fields[3] == "y":
            found_master = True
            break
    if not found_master:
        raise AssertionError(
            f"halmasuit PID {started_pid} is not DRM master post-pivot.\n"
            f"debugfs clients:\n{drm_clients}"
        )
    print(f"PASS: PID {started_pid} holds DRM master post-pivot")

    # ASSERTION 5: RootfsReady was emitted (proves the pivot-poll
    # calloop source fired — `/etc/initrd-release` was observed
    # disappearing).
    if not rootfs_ready_seen:
        raise AssertionError(
            "halmasuit did NOT emit `phase: rootfs_ready` — the pivot-"
            "poll calloop source may not have fired, or /etc/initrd-"
            "release did not disappear as expected.\n"
            "Phase events in journal:\n"
            + machine.execute("journalctl -b --output=cat --no-pager | grep -F 'phase_entered' || echo 'no phase_entered events found'")[1]
        )
    print("PASS: phase=rootfs_ready emitted (pivot detected post-switch_root)")

    # ASSERTION 6: Wayland socket bound and present at the expected
    # path. /run/halmasuit/ is a tmpfs created by the unit's
    # RuntimeDirectory directive in initramfs; /run is `mount --move`d
    # during switch_root, so the directory and the socket persist.
    socket_check = machine.execute("test -S /run/halmasuit/wayland-0")[0]
    if socket_check != 0:
        raise AssertionError(
            "halmasuit's Wayland socket is missing at "
            "/run/halmasuit/wayland-0 post-pivot.\n"
            "Contents of /run/halmasuit/:\n"
            + machine.execute("ls -la /run/halmasuit/ 2>&1 || echo 'directory missing'")[1]
        )
    print("PASS: Wayland socket present at /run/halmasuit/wayland-0 post-pivot")

    print(
        f"initrd-survival: halmasuit PID {started_pid} survived switch_root, "
        "holds DRM master direct, emitted single NDJSON event stream "
        "spanning both phases, Wayland socket bound post-pivot"
    )
  '';
}
