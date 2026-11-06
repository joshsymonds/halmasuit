# tests/halmasuit-render-node-eval.nix — pure-eval gate for
# `services.halmasuit.rendering.renderNode`.
#
# Contract: when renderNode is non-null, both the rootfs
# `halmasuit.service` and the initramfs `halmasuit.service` carry
# `HALMASUIT_RENDER_NODE=<value>`. When null, neither carries the key,
# and every other env entry is byte-identical to the set case.
#
# Implementation: two synthetic `nixosSystem` evaluations of the
# halmasuit module (fromInitrd enabled), one with a render node and one
# with null. Each unit's environment is flattened to `KEY=value` lines
# and compared with grep/cmp in a `runCommand`. No VM, no boot.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-luks,
}:

let
  pkgs = import nixpkgs { inherit system; };

  renderNodeValue = "/dev/dri/renderD128";

  evalWith = renderNode: nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ../nix/module.nix
      ({ ... }: {
        services.halmasuit = {
          fromInitrd.enable = true;
          package           = halmasuit;
          session.package   = halmasuit-session;
          luks.package      = halmasuit-luks;
          greeterUid        = 999;
          greeterGroup      = "halmasuit-greeter";
          compositorUid     = 998;

          rendering.renderNode = renderNode;
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

        # Required-option stubs. Nothing reads these.
        fileSystems."/" = { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
        boot.loader.systemd-boot.enable = true;
        system.stateVersion = "26.05";
      })
    ];
  };

  setCfg  = (evalWith renderNodeValue).config;
  nullCfg = (evalWith null).config;

  envFlat = unit:
    builtins.concatStringsSep "\n"
      (pkgs.lib.mapAttrsToList (k: v: "${k}=${toString v}") unit.environment);

  setRootfsEnv  = envFlat setCfg.systemd.services.halmasuit;
  setInitrdEnv  = envFlat setCfg.boot.initrd.systemd.services.halmasuit;
  nullRootfsEnv = envFlat nullCfg.systemd.services.halmasuit;
  nullInitrdEnv = envFlat nullCfg.boot.initrd.systemd.services.halmasuit;
in
pkgs.runCommand "halmasuit-render-node-eval" { } ''
  set -eu

  mkdir -p $out

  cat > $out/set-rootfs.env <<'EOF'
${setRootfsEnv}
EOF

  cat > $out/set-initrd.env <<'EOF'
${setInitrdEnv}
EOF

  cat > $out/null-rootfs.env <<'EOF'
${nullRootfsEnv}
EOF

  cat > $out/null-initrd.env <<'EOF'
${nullInitrdEnv}
EOF

  fail() {
    for f in set-rootfs set-initrd null-rootfs null-initrd; do
      echo "──── $f.env ────"
      cat $out/$f.env
    done
    echo "FAIL: $1"
    exit 1
  }

  # ── (1) Set: both units export the render node ───────────────────
  grep -qxF 'HALMASUIT_RENDER_NODE=${renderNodeValue}' $out/set-rootfs.env \
    || fail "rootfs halmasuit.service missing HALMASUIT_RENDER_NODE=${renderNodeValue}"
  grep -qxF 'HALMASUIT_RENDER_NODE=${renderNodeValue}' $out/set-initrd.env \
    || fail "initramfs halmasuit.service missing HALMASUIT_RENDER_NODE=${renderNodeValue}"

  # ── (2) Null: neither unit carries the key ───────────────────────
  grep -q '^HALMASUIT_RENDER_NODE=' $out/null-rootfs.env \
    && fail "null: rootfs halmasuit.service still carries HALMASUIT_RENDER_NODE"
  grep -q '^HALMASUIT_RENDER_NODE=' $out/null-initrd.env \
    && fail "null: initramfs halmasuit.service still carries HALMASUIT_RENDER_NODE"

  # ── (3) Null leaves every other env entry unchanged ──────────────
  grep -v '^HALMASUIT_RENDER_NODE=' $out/set-rootfs.env > $out/set-rootfs-minus.env
  grep -v '^HALMASUIT_RENDER_NODE=' $out/set-initrd.env > $out/set-initrd-minus.env
  cmp -s $out/set-rootfs-minus.env $out/null-rootfs.env \
    || fail "null rootfs env differs from set rootfs env beyond HALMASUIT_RENDER_NODE"
  cmp -s $out/set-initrd-minus.env $out/null-initrd.env \
    || fail "null initramfs env differs from set initramfs env beyond HALMASUIT_RENDER_NODE"

  echo "halmasuit-render-node-eval: PASS"
''
