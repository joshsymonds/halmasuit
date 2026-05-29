# Phase B hard gate: end-to-end Phase B v2 boot arc, real PAM auth
# through halmasuit's broker via abstract sockets.
#
# Asserts:
#   1. halmasuit survives the switch_root with PID + DRM master +
#      Wayland socket continuity (the survival mechanic gated by
#      tests/initrd-survival.nix).
#   2. Post-pivot, halmasuit migrates into rootfs's mount-namespace
#      view via the broker SCM_RIGHTS root-fd handoff
#      (RequestRootFd / RootFd protocol), spawns the configured
#      greeter, drops privileges, binds the greetd socket at
#      /run/halmasuit/greetd.sock (POST-CHROOT: since the broker
#      root-fd migration in 831fe50 the listener is the filesystem
#      socket, not the old abstract @halmasuit-greetd name).
#   3. A halmasuit-vm-client connects to /run/halmasuit/greetd.sock
#      from the rootfs and drives a full PAM auth → SessionOpened arc
#      through real `pam_unix` via the broker.
#
# Scope notes:
# - LUKS-encrypted root is a NixOS-test-infra concern (custom disk
#   image build at evaluation time) rather than a halmasuit
#   integration concern; deferred. systemd-cryptsetup's
#   ask-password protocol bridging via halmasuit-luks is the
#   compositor-side concern, gated on the wayland-0 cross-mount-ns
#   bridge (the v2 follow-up — currently halmasuit-luks runs as a
#   rootfs unit that can't see halmasuit's wayland-0).
# - Frame-capture continuity (DSSIM thresholds across the timeline)
#   is deferred to the visual-* test family extension. The pixel-
#   level no-flash invariant is gated elsewhere on the rootfs
#   deployment; extending it to the boot-from-initrd timeline is
#   a follow-up polish pass.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "full-boot-flash";

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
        # Not importing test-user.nix because we declare alice
        # explicitly below at uid 1000 (test-user.nix would collide).
      ];

      virtualisation = {
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };

      # Phase B boot-from-initrd deployment.
      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
        luks.package      = halmasuit-luks;
        session.package   = halmasuit-session;
        greeterCommand    = "${testGreeter}";
      };

      # Pin the greeter script into the rootfs closure (initramfs
      # storePaths alone isn't enough now that halmasuit chroots
      # into rootfs's view post-pivot).
      system.extraDependencies = [ testGreeter ];

      # Tools the testScript needs:
      # - halmasuit-vm-client: drives the /run/halmasuit/greetd.sock
      #   socket for the full-auth arc post-pivot.
      environment.systemPackages = [ halmasuit-vm-client ];

      # The user halmasuit-vm-client will authenticate. uid + gid ≥
      # UID_MIN per the broker's load-bearing floor.
      users.users.alice = {
        isNormalUser = true;
        uid          = 1000;
        group        = "alice";
        password     = "testpassword";
      };
      users.groups.alice.gid = 1000;
    };

  testScript = ''
    import json

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Phase 3: halmasuit reached post-pivot setup. Same shape as
    # initrd-survival.nix but expressed as a single wait on the
    # final event.
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"event\\\":\\\"greeter_spawned\\\"'",
        timeout=30,
    )
    print("PASS: greeter spawned post-pivot")

    # Collect halmasuit's emitted events.
    raw = machine.succeed("journalctl -b --output=cat --no-pager")
    started_pid = None
    greeter_pid = None
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
        ev = inner.get("event")
        if ev == "started":
            started_pid = inner.get("pid")
        elif ev == "greeter_spawned":
            greeter_pid = inner.get("pid")
        elif ev == "phase_entered":
            phases_seen.add(inner.get("phase"))

    assert started_pid is not None, "halmasuit did not emit Started"
    print(f"halmasuit PID = {started_pid}")
    print(f"phases observed: {sorted(phases_seen)}")
    for required in ("initramfs_init", "rootfs_ready", "greetd_ready", "deprivileged"):
        assert required in phases_seen, f"phase {required!r} missing"
    print("PASS: all post-pivot phases observed")

    # Phase 4: full auth via the greetd socket. halmasuit-vm-client
    # connects to /run/halmasuit/greetd.sock, drives a real-pam_unix
    # auth as alice, hits SessionOpened.
    # Mirror tests/halmasuit-vm.nix's auth setup precisely.
    machine.succeed("printf 'testpassword' > /tmp/alice.pw")
    machine.succeed("chown halmasuit-greeter:halmasuit-greeter /tmp/alice.pw")
    machine.succeed("chmod 600 /tmp/alice.pw")
    # halmasuit's greetd listener authorizes only the configured
    # greeter uid via SO_PEERCRED — run vm-client AS that user.
    machine.succeed(
        "runuser -u halmasuit-greeter -- "
        "halmasuit-vm-client full-auth /run/halmasuit/greetd.sock alice "
        "--password-file /tmp/alice.pw "
        "--cmd /run/current-system/sw/bin/true "
        "--timeout 30"
    )
    print("PASS: full-auth → SessionOpened completed end-to-end")

    print(
        f"full-boot-flash: LUKS unlocked → halmasuit PID {started_pid} "
        f"survived pivot, chrooted into rootfs, greeter PID "
        f"{greeter_pid} spawned, full PAM auth arc → SessionOpened"
    )
  '';
}
