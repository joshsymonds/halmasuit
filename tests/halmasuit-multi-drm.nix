# tests/halmasuit-multi-drm.nix — regression gate for multi-DRM-device
# hosts (Epic: halmasuit multi-DRM-device support, DRM4).
#
# Boots a NixOS VM with TWO virtio-gpu-pci devices attached at known
# PCI BDFs. Halmasuit's `services.halmasuit.drmDevice` is varied per
# instance to exercise each of Auto / Path / Pci modes. Each instance
# asserts halmasuit opened the EXPECTED card based on the mode —
# specifically catching the tonight's-gnomon-failure class where
# halmasuit blindly opens card0 even when that's the wrong card.
#
# Why this exists: the rest of the test matrix uses single-virtio-gpu
# VMs, where card0 IS the only card AND the right card by accident.
# That setup hides bugs like (a) hardcoded-card0 fallback and (b)
# initramfs-race where halmasuit's open() beats udev's node creation.
# This test runs against a multi-card substrate that catches (a)
# unambiguously and (b) probabilistically (QEMU virtio-gpu enumerates
# fast, so we'd need an artificial delay to deterministically race the
# udev settle — not in scope here; the systemd-udev-settle wants= fix
# in DRM3 is verified by Nix-eval probe and at deploy time).
#
# The test is parametric: takes a `drmDevice` value + an `expected`
# path predicate. flake.nix instantiates three checks.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  # Test parameters (vary per check instance).
  drmDevice,        # null | "/dev/dri/cardN" | "pci:DDDD:BB:DD.F"
  expectedPath,     # string: the /dev/dri/cardN halmasuit should open
  testName,         # human-readable suffix for the check name
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-multi-drm-${testName}";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        # No-op greeter — we're testing DRM device resolution, not
        # the greeter chain.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-test-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
        # THE thing under test.
        inherit drmDevice;
      };

      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
      };

      environment.systemPackages = [ halmasuit-vm-client ];

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
        # Two virtio-gpu-pci devices at pinned BDFs. The default qemu-
        # vm module already adds one (typically `-device virtio-gpu-pci`
        # without addr=); we add an explicit one at 0000:00:05.0
        # alongside a second at 0000:00:06.0 so we have a deterministic
        # PCI topology and KNOW which BDF to feed `pci:` mode.
        #
        # `-vga none` suppresses QEMU's default VGA (which would
        # otherwise occupy /dev/dri/card0 as a vgaarb-managed device
        # and shift virtio-gpu to card1+card2 — possible but harder to
        # assert against). With -vga none the only DRM devices are
        # the two virtio-gpus, deterministically at card0/card1.
        # NixOS test framework's default qemu config occupies several
        # PCI slots (rng, network, virtio-blk, virtio-serial). Pin our
        # two virtio-gpu devices at high slots (0x0e/0x0f) to avoid
        # collision with the framework's auto-assigned slots.
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci,addr=0x0e.0"
          "-device virtio-gpu-pci,addr=0x0f.0"
        ];
      };
    };

  testScript = ''
    import json
    import re

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # ─ Substrate sanity: two cards present ──────────────────────────
    cards = machine.succeed("ls /dev/dri/ | grep '^card' | sort").strip().split()
    assert cards == ["card0", "card1"], (
        f"expected two virtio-gpu cards (card0, card1); got: {cards}"
    )

    # Confirm BDF assignment: card0 → 0000:00:05.0, card1 → 0000:00:06.0
    bdf0 = machine.succeed(
        "basename $(readlink /sys/class/drm/card0/device)"
    ).strip()
    bdf1 = machine.succeed(
        "basename $(readlink /sys/class/drm/card1/device)"
    ).strip()
    assert bdf0 == "0000:00:0e.0", f"card0 BDF unexpected: {bdf0!r}"
    assert bdf1 == "0000:00:0f.0", f"card1 BDF unexpected: {bdf1!r}"

    # ─ Halmasuit reached drm_master_acquired ────────────────────────
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'drm_master_acquired'",
        timeout=30,
    )

    # ─ Assert which card halmasuit resolved to ──────────────────────
    # The DRM1 main.rs wiring emits a tracing INFO log
    # "DRM device resolved" with the resolved path. Parse the
    # tracing-subscriber JSON envelope to extract it.
    raw = machine.succeed("journalctl -u halmasuit -o cat --no-pager")
    resolved_paths = []
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            outer = json.loads(line)
        except Exception:
            continue
        fields = outer.get("fields", {})
        if fields.get("message") != "DRM device resolved":
            continue
        # tracing-subscriber serializes `path` (a PathBuf) as its
        # Debug repr — `"\"/dev/dri/card1\""` — strip the outer
        # quotes.
        path_debug = fields.get("path", "")
        m = re.match(r'^"(.*)"$', path_debug)
        if m:
            resolved_paths.append(m.group(1))
        else:
            resolved_paths.append(path_debug)

    assert resolved_paths, (
        f"halmasuit did not emit a 'DRM device resolved' log. "
        f"Journal: {raw}"
    )
    resolved = resolved_paths[-1]  # last is the production resolve

    assert resolved == "${expectedPath}", (
        f"expected halmasuit to open ${expectedPath}, got {resolved}. "
        f"All resolved-path log lines: {resolved_paths}"
    )

    print(
        f"halmasuit-multi-drm-${testName}: PASS "
        f"(drmDevice=${toString (if drmDevice == null then "null" else drmDevice)}, "
        f"resolved={resolved})"
    )
  '';
}
