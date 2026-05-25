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
#   7. Post-pivot, halmasuit emitted `phase: greetd_ready` (the
#      `run_post_pivot_setup` greetd-listener bind succeeded).
#   8. Post-pivot, halmasuit emitted `phase: deprivileged` (the
#      `run_post_pivot_setup` privilege drop to the compositor uid
#      succeeded).
#
# This composes the Phase 2 survival mechanism (RESEARCH.md L86-87)
# with the post-pivot transition: same PID across switch_root, drops
# to the unprivileged compositor uid only after the rootfs is alive
# and `/etc/passwd` resolves.
#
# Out of scope here (Phase B follow-up):
# - LUKS volume actually unlocked (full-boot-flash.nix)
# - Frame-capture continuity (full-boot-flash.nix)

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-luks,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "initrd-survival";

  nodes.machine =
    { config, lib, pkgs, ... }:
    let
      testGreeter = pkgs.writeShellScript "halmasuit-test-greeter" ''
        exec ${pkgs.coreutils}/bin/sleep infinity
      '';
    in
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
        luks.package      = halmasuit-luks;
        # Broker shipped alongside the compositor; the
        # `fromInitrd.enable` deployment auto-provisions the broker
        # unit via the same module block the rootfs `enable` path uses.
        session.package   = halmasuit-session;
        # No greeterUid / compositorUid / greeterGroup overrides:
        # the module's defaults (999/998/"halmasuit-greeter") and the
        # auto-created system users are what production deployments
        # get out-of-the-box, so the test exercises that surface too.
        # Sleep-forever greeter — the test asserts greeter SPAWN, not
        # greeter UI fidelity (that's the rootfs visual tests' job).
        # The let-binding pins the same derivation in
        # `system.extraDependencies` below, ensuring it lands in
        # ROOTFS's nix store (otherwise the script would only be in
        # initramfs's closure and halmasuit, after chrooting to
        # rootfs's view, couldn't find it).
        greeterCommand = "${testGreeter}";
      };

      system.extraDependencies = [ testGreeter ];

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

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Confirm the test-greeter script is in rootfs's nix store.
    test_greeter_check = machine.execute(
        "ls /nix/store/*halmasuit-test-greeter 2>&1 | head -3"
    )
    print(f"rootfs nix store sees test-greeter: {test_greeter_check[1]}")


    # Wait for post-pivot setup to complete: halmasuit's pivot-poll
    # fires once /etc/initrd-release is gone, then `run_post_pivot_setup`
    # binds greetd, spawns the greeter, and drops privileges. Total
    # window ~2-5s post-pivot; 15s here is generous headroom.
    #
    # The journalctl `MESSAGE` field carries the outer
    # tracing-subscriber JSON envelope; the halmasuit-introspect inner
    # event JSON appears with backslash-escaped quotes there. Pattern-
    # match the escaped form via grep -F.
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"phase\\\":\\\"deprivileged\\\"'",
        timeout=15,
    )

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

    # Walk the NDJSON event stream once, collect every event we care
    # about. The unit lived in initramfs systemd, so by the time
    # rootfs is up the unit is no longer registered with rootfs
    # systemd — `journalctl -u` won't find it. But the JSON log
    # lines persist in journald (cross-pivot journald continuity is
    # the whole point of structured logging).
    raw = machine.succeed("journalctl -b --output=cat --no-pager")
    started_pid = None
    phases_seen = set()
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
            phases_seen.add(inner.get("phase"))

    if started_pid is None:
        raise AssertionError(
            "halmasuit did NOT emit a `Started` event — the unit may not "
            "have launched in initramfs.\n"
            "Tail of journal:\n"
            + machine.succeed("journalctl -b --output=cat --no-pager | tail -50")
        )
    print(f"halmasuit PID = {started_pid}")
    print(f"phases observed: {sorted(phases_seen)}")

    # ASSERTION 2: InitramfsInit was emitted (proves the
    # `is_initramfs()` branch fired).
    assert "initramfs_init" in phases_seen, (
        "halmasuit did NOT emit `phase: initramfs_init` — the "
        "runtime-detection branch in main.rs may not have fired."
    )
    print("PASS: phase=initramfs_init emitted")

    # ASSERTION 3: PID survived the pivot.
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
    assert "rootfs_ready" in phases_seen, (
        "halmasuit did NOT emit `phase: rootfs_ready` — the pivot-"
        "poll calloop source may not have fired, or /etc/initrd-"
        "release did not disappear as expected.\n"
        f"phases: {sorted(phases_seen)}"
    )
    print("PASS: phase=rootfs_ready emitted (pivot detected post-switch_root)")

    # ASSERTION 6: Wayland socket bound and present at the expected
    # path. /run is a tmpfs `mount --move`d during switch_root, so
    # the directory and the socket persist.
    socket_check = machine.execute("test -S /run/halmasuit/wayland-0")[0]
    if socket_check != 0:
        raise AssertionError(
            "halmasuit's Wayland socket is missing at "
            "/run/halmasuit/wayland-0 post-pivot.\n"
            "Contents of /run/halmasuit/:\n"
            + machine.execute("ls -la /run/halmasuit/ 2>&1 || echo 'directory missing'")[1]
        )
    print("PASS: Wayland socket present at /run/halmasuit/wayland-0 post-pivot")

    # ASSERTION 7: post-pivot greetd listener bound AND reachable
    # from the rootfs view. The Phase B v1 → v2 fix uses an ABSTRACT
    # Linux socket (`@halmasuit-greetd`) instead of a filesystem
    # path — abstract sockets live in the NETWORK namespace which
    # halmasuit + rootfs share, so cross-mount-namespace visibility
    # of the listener is no longer a problem.
    assert "greetd_ready" in phases_seen, (
        "halmasuit did NOT emit `phase: greetd_ready` post-pivot — "
        "`run_post_pivot_setup` either didn't run or failed to bind "
        "the greetd socket.\n"
        f"phases: {sorted(phases_seen)}"
    )
    # Check the kernel's listening Unix sockets from the rootfs net
    # namespace (which halmasuit + rootfs share). If the abstract
    # name appears as a LISTEN socket, halmasuit's bind is visible
    # cross-mount-ns. /proc/net/unix lists abstract sockets with `@`
    # prefix in the Path column.
    abstract_check = machine.execute(
        "grep -E '@halmasuit-greetd' /proc/net/unix || echo 'not-found'"
    )
    assert "not-found" not in abstract_check[1], (
        f"abstract @halmasuit-greetd socket NOT visible from rootfs view.\n"
        f"output: {abstract_check[1]}\n"
        "Mismatch suggests the bind happened in halmasuit's own net "
        "ns or didn't happen at all."
    )
    print("PASS: abstract @halmasuit-greetd socket reachable from rootfs view")

    # ASSERTION 8: post-pivot privilege drop completed. Proves
    # `drop_privileges(compositorUid)` ran successfully — halmasuit
    # is no longer root. The /proc/<pid>/status check is the load-
    # bearing security assertion: the compositor MUST run as the
    # configured compositor uid post-pivot (default 998, auto-created
    # by the module).
    assert "deprivileged" in phases_seen, (
        "halmasuit did NOT emit `phase: deprivileged` post-pivot — "
        "the drop to the compositor uid either didn't run or failed.\n"
        f"phases: {sorted(phases_seen)}"
    )
    status_uid = machine.succeed(f"grep '^Uid:' /proc/{started_pid}/status").strip()
    uid_fields = status_uid.split()
    assert all(f == "998" for f in uid_fields[1:5]), (
        f"halmasuit PID {started_pid} has Uid fields {uid_fields[1:5]}; "
        "expected all 998 (the compositor uid default from the module). "
        "The deprivileged event fired but setresuid didn't take."
    )
    print(f"PASS: phase=deprivileged emitted; PID {started_pid} runs as compositor uid 998")

    # ASSERTION 9: post-pivot greeter spawn succeeded. Proves
    # halmasuit's process-root migration via the broker-mediated
    # SCM_RIGHTS fd transfer worked end-to-end: halmasuit can now see
    # rootfs's /etc/passwd (getpwuid), /nix/store (greeter binary),
    # and exec'd the greeter as a child.
    raw_greeter = machine.succeed("journalctl -b --output=cat --no-pager")
    greeter_pid = None
    for line in raw_greeter.splitlines():
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
        if inner.get("event") == "greeter_spawned":
            greeter_pid = inner.get("pid")
            break
    assert greeter_pid is not None, (
        "halmasuit did NOT emit `greeter_spawned` post-pivot. "
        "Either run_post_pivot_setup's spawn_greeter failed (check "
        "broker root-fd migration + chroot), or the greeter binary "
        "isn't in rootfs's /nix/store closure."
    )
    # Greeter must be alive at test time.
    alive = machine.execute(f"kill -0 {greeter_pid}")[0]
    assert alive == 0, f"greeter PID {greeter_pid} not alive"
    print(f"PASS: post-pivot greeter spawned (PID {greeter_pid}); chroot to rootfs view succeeded")

    print(
        f"initrd-survival: halmasuit PID {started_pid} survived switch_root, "
        "holds DRM master direct, completed post-pivot setup (greetd_ready, "
        "deprivileged), Wayland socket bound post-pivot"
    )
  '';
}
