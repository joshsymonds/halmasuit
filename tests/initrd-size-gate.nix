# tests/initrd-size-gate.nix — boot-size regression gate.
#
# Builds a Phase B halmasuit config and asserts the resulting
# initramfs is under threshold. Catches closure regressions BEFORE
# they hit deployment-time ESP-overflow on a signed UKI partition.
#
# What this measures:
#   The contents of `boot.initrd.systemd.storePaths` resolved through
#   `config.system.build.initialRamdisk`. That covers everything
#   halmasuit's NixOS module bakes into initramfs: the halmasuit
#   binary + halmasuit-luks + mesa OR the nvidia closure + libglvnd
#   + xkeyboard-config + wallpaper assets + decoder (if video) +
#   any `rendering.extraInitrdStorePaths`. The post-pivot rootfs
#   surface (niri + DMS + Quickshell) is NOT baked into initramfs —
#   that lives in the rootfs nix store and is reached after the
#   pivot, so it doesn't affect initramfs size.
#
# Threshold maintenance:
#   The threshold is intentionally in-source. When a legitimate
#   closure-growing change lands (a new wayland-protocols version,
#   an added systemd module, etc.), the gate WILL fail. The
#   implementer bumps the threshold in the same PR and documents the
#   reason in the commit message. The friction is the point —
#   automatic threshold updates would defeat the gate.
#
# Investigation recipe when this fires:
#   nix path-info -rsSh <initrd-out-path> | sort -k2 -h | tail -30
#   → shows the 30 largest store paths in the initramfs closure.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-luks,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };

  # Minimal Phase B halmasuit config — the only inputs that affect
  # initramfs size are the ones halmasuit's module bakes into
  # `boot.initrd.systemd.storePaths`. The default rendering backend
  # ("software") is the gnomon-portable baseline; the nvidia backend
  # would produce a different (larger) initramfs but isn't testable
  # here without nvidia-drm hardware, and the threshold tracks the
  # portable baseline by design.
  testSystem = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ../nix/module.nix
      ({ ... }: {
        nixpkgs.config.allowUnfree = true;

        boot.loader.systemd-boot.enable = true;
        boot.loader.efi.canTouchEfiVariables = false;
        fileSystems."/" = { device = "/dev/null"; fsType = "ext4"; };
        system.stateVersion = "25.05";

        services.halmasuit.fromInitrd.enable = true;
        services.halmasuit.greeterCommand   = "/bin/true";
        services.halmasuit.package          = halmasuit;
        services.halmasuit.luks.package     = halmasuit-luks;
        services.halmasuit.session.package  = halmasuit-session;

        # The wallpaper plane is part of the gnomon-shaped baseline.
        # Use a small fixture image — the wallpaper FILE goes into
        # storePaths but the size contribution is bounded by the
        # actual image bytes, not the test config.
        services.halmasuit.wallpaper = {
          type   = "image";
          source = ./fixtures/wallpaper.png;
        };
      })
    ];
  };

  # Threshold: measured baseline + ~26% headroom.
  #
  # MEASURED BASELINE (2026-05-28): 146,213,668 bytes (139 MiB).
  # Threshold below: 184,549,376 bytes (176 MiB) — rounded up to a
  # clean MiB boundary, which gives 184549376 / 146213668 = 1.262×
  # baseline = ~26% headroom.
  #
  # Bump deliberately when a legitimate closure addition lands;
  # document the WHY in the commit message bumping this value.
  thresholdBytes = 184549376;

  initrd = testSystem.config.system.build.initialRamdisk;
in
pkgs.runCommand "halmasuit-initrd-size-gate"
  {
    inherit initrd;
    nativeBuildInputs = [ pkgs.coreutils ];
    passthru = { inherit thresholdBytes; };
  } ''
    set -euo pipefail

    # The initialRamdisk derivation typically exposes the image at
    # $initrd/initrd; tolerate either that or a direct file output.
    if [ -f "$initrd/initrd" ]; then
      target="$initrd/initrd"
    elif [ -f "$initrd" ]; then
      target="$initrd"
    else
      echo "ERROR: cannot find initramfs image under $initrd:"
      ls -la "$initrd" || true
      exit 1
    fi

    size=$(stat -c %s "$target")
    thresholdMiB=$(( ${toString thresholdBytes} / 1024 / 1024 ))
    sizeMiB=$(( size / 1024 / 1024 ))

    echo "Phase B initramfs: $size bytes ($sizeMiB MiB)"
    echo "Threshold:         ${toString thresholdBytes} bytes ($thresholdMiB MiB)"

    if [ "$size" -gt ${toString thresholdBytes} ]; then
      echo
      echo "GATE FAILED: initramfs size exceeds threshold by $(( size - ${toString thresholdBytes} )) bytes."
      echo
      echo "Investigate via:"
      echo "  nix path-info -rsSh $initrd | sort -k2 -h | tail -30"
      echo
      echo "If the growth is legitimate (new dep, version bump), bump"
      echo "thresholdBytes in tests/initrd-size-gate.nix and document"
      echo "the reason in the commit message."
      exit 1
    fi
    echo "GATE PASSED."
    touch "$out"
  ''
