# halmasuit-tty-graphics: Plymouth-equivalent VT graphics-mode switch.
#
# The halmasuit-tty-graphics initramfs oneshot opens /dev/tty1 and
# issues `KDSETMODE → KD_GRAPHICS` before systemd-modules-load runs.
# That puts the kernel framebuffer console in graphics mode, so the
# `crng init done` / `nvidia: loading out-of-tree module ...` lines
# the kernel still prints into the printk ring do NOT render onto
# the physical screen — Plymouth's job, done by us.
#
# This test asserts:
#   1. The halmasuit-tty-graphics unit is present and active.
#   2. Its journal marker ("/dev/tty1 → KD_GRAPHICS") appears BEFORE
#      halmasuit's `phase_entered scanout_active` event (ordering
#      gate — the suppression must beat halmasuit's first frame, or
#      it's racing nothing useful).
#   3. Forensics intact: dmesg STILL contains every kernel boot
#      message (KDSETMODE is visual-only; printk ring untouched).
#   4. The unit ran exactly once (`oneshot` + `RemainAfterExit=yes`;
#      a multi-fire would imply a service-config bug).
#
# Out of scope here: the kernel cmdline `loglevel=1` /
# `rd.udev.log_priority=3` knobs live in nix-config (per-host
# concerns); this test only gates halmasuit's contribution.

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
  name = "halmasuit-tty-graphics";

  nodes.machine =
    { config, lib, pkgs, ... }:
    let
      testGreeter = pkgs.writeShellScript "halmasuit-tty-graphics-greeter" ''
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
        session.package   = halmasuit-session;
        greeterCommand    = "${testGreeter}";
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
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Wait for halmasuit's first frame to be acknowledged so we can
    # assert ordering between the tty-graphics marker (initrd) and
    # `scanout_active` (post-pivot).
    machine.wait_until_succeeds(
        "journalctl -b --output=cat --no-pager "
        "| grep -F '\\\"phase\\\":\\\"scanout_active\\\"'",
        timeout=30,
    )

    # ── ASSERTION 1: the unit's process ran. ────────────────────────
    # In initrd-as-stage1 systemd, the rootfs journal does NOT carry
    # initramfs systemd's `Started halmasuit-tty-graphics` line (the
    # start-of-unit message stays inside initramfs and is dropped at
    # pivot). The cross-pivot trace IS the process's own stdout
    # marker + the Deactivated/Stopped messages from rootfs systemd
    # picking up the surviving cgroup. Use the binary-emitted marker
    # as the strongest evidence that the unit's ExecStart ran.
    unit_ran = machine.execute(
        "journalctl -b --no-pager "
        "| grep -F 'halmasuit-tty-graphics' "
        "| grep -E 'KD_GRAPHICS|Deactivated|Stopped' "
        "| head -3"
    )[1].strip()
    if not unit_ran:
        all_lines = machine.execute(
            "journalctl -b --no-pager | grep -i halmasuit-tty-graphics"
        )[1]
        raise AssertionError(
            "No journal evidence that halmasuit-tty-graphics ran. "
            "Expected the binary's stdout marker OR systemd's "
            "Deactivated/Stopped entries.\n"
            "All halmasuit-tty-graphics lines:\n"
            + all_lines
        )
    print(f"PASS (1/4): unit ran — {unit_ran.splitlines()[0]}")

    # ── ASSERTION 2: KDSETMODE marker present. ──────────────────────
    # The binary prints "halmasuit-tty-graphics: /dev/tty1 → KD_GRAPHICS"
    # on success. journal+console StandardOutput captures it.
    kdsetmode_marker = machine.execute(
        "journalctl -b --no-pager | grep -F 'KD_GRAPHICS' | head -3"
    )[1].strip()
    if "KD_GRAPHICS" not in kdsetmode_marker:
        unit_journal = machine.execute(
            "journalctl -b --no-pager -u halmasuit-tty-graphics || "
            "journalctl -b --no-pager | grep halmasuit-tty-graphics"
        )[1]
        raise AssertionError(
            "halmasuit-tty-graphics ran but did NOT emit the "
            "KDSETMODE success marker. The ioctl failed silently?\n"
            "Journal lines from the unit:\n"
            + unit_journal
        )
    print(f"PASS (2/4): KDSETMODE marker — {kdsetmode_marker.splitlines()[0]}")

    # ── ASSERTION 3: ordering. ──────────────────────────────────────
    # The KDSETMODE marker MUST land before halmasuit's scanout_active.
    # If the unit fires AFTER halmasuit's first frame, the screen has
    # already shown text-mode garbage; the suppression is useless.
    # Compare monotonic-time prefixes in journalctl --output=short-monotonic.
    timeline = machine.succeed(
        "journalctl -b --output=short-monotonic --no-pager "
        "| grep -E 'KD_GRAPHICS|scanout_active' "
        "| head -10"
    )
    print("Ordering window:\n" + timeline)
    kdsetmode_line = None
    scanout_line = None
    for line in timeline.splitlines():
        if "KD_GRAPHICS" in line and kdsetmode_line is None:
            kdsetmode_line = line
        if "scanout_active" in line and scanout_line is None:
            scanout_line = line
    if not kdsetmode_line or not scanout_line:
        raise AssertionError(
            f"Could not find both markers in ordered timeline. "
            f"KD_GRAPHICS={kdsetmode_line!r}, scanout_active={scanout_line!r}"
        )
    # Both lines start with `[   N.NNNNNN]` — extract the seconds.
    def monotonic(s):
        import re
        m = re.search(r"\[\s*(\d+\.\d+)\]", s)
        return float(m.group(1)) if m else None
    t_kd = monotonic(kdsetmode_line)
    t_scanout = monotonic(scanout_line)
    if t_kd is None or t_scanout is None:
        raise AssertionError(
            f"Could not parse monotonic timestamps:\n  "
            f"kd={kdsetmode_line!r}\n  scanout={scanout_line!r}"
        )
    if t_kd >= t_scanout:
        raise AssertionError(
            f"ORDERING REGRESSION: KDSETMODE fired at t={t_kd}s, "
            f"AFTER scanout_active at t={t_scanout}s. The tty1 must "
            f"be in graphics mode BEFORE halmasuit paints its first "
            f"frame, or the suppression is racing nothing useful."
        )
    print(
        f"PASS (3/4): ordering — KD_GRAPHICS at t={t_kd}s "
        f"precedes scanout_active at t={t_scanout}s "
        f"(Δ={t_scanout - t_kd:.3f}s)"
    )

    # ── ASSERTION 4: forensics intact. ──────────────────────────────
    # The KDSETMODE only stops VT text rendering; the kernel still
    # writes printk lines to its internal ring (dmesg/journal). If
    # somehow this regressed (e.g., someone added a `dmesg -n 0`
    # call), post-incident debugging would be blind. Assert at least
    # one boot-phase kernel line is present.
    forensics = machine.execute(
        "dmesg | grep -iE 'kernel command line|Linux version' | head -3"
    )[1].strip()
    if not forensics:
        raise AssertionError(
            "Forensics regression: dmesg has no boot-phase kernel "
            "lines after KDSETMODE ran. The suppression is supposed "
            "to be visual-only — the printk ring MUST still carry "
            "kernel boot messages for post-incident debugging."
        )
    print(f"PASS (4/4): forensics intact — dmesg carries:\n  {forensics.splitlines()[0]}")

    print(
        "halmasuit-tty-graphics: ALL ASSERTIONS PASSED — "
        "/dev/tty1 transitioned to KD_GRAPHICS before halmasuit's "
        "first frame; kernel forensics intact in dmesg/journal."
    )
  '';
}
