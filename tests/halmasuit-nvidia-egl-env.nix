# tests/halmasuit-nvidia-egl-env.nix — regression gate for the
# `__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS` env var AND the
# `nvidia-devnodes` ExecStartPre wiring on the nvidia rendering
# backend.
#
# Why this exists: we cannot replicate the actual NVIDIA-EGL failure
# in CI (consumer Blackwell + single GPU has no passthrough route to
# a guest VM; the headless test substrate is virtio-gpu-pci). What we
# CAN catch deterministically is the SHAPE of the unit — every
# regression we've hit so far that this gate covers showed up as
# either (a) the wrong/missing env var on the halmasuit.service
# definition or (b) a writeShellScript referenced from the unit but
# not baked into the initramfs storePaths closure.
#
# Implementation: pure Nix-eval. Builds a synthetic `nixosSystem` with
# the halmasuit module + nvidia backend + fromInitrd, reads the
# generated initramfs halmasuit.service unit and the storePaths list
# directly out of the evaluated config, and asserts string contents
# via a `runCommand` shell test. No VM, no kernel boot, no waiting on
# multi-user.target — runs in ~3-5 seconds.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-luks,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # Stand-ins for the real NVIDIA closure. Mesa has the right
  # share/glvnd/egl_vendor.d shape to make the module eval pass; we
  # only care about the OUTGOING env vars + ExecStartPre wiring.
  stubNvidiaPackage   = pkgs.mesa;
  stubExtraStorePaths = [ pkgs.egl-wayland pkgs.egl-gbm ];

  expectedConfigDirs = pkgs.lib.makeSearchPath
    "share/egl/egl_external_platform.d"
    stubExtraStorePaths;

  # Evaluate the halmasuit module against a synthetic NixOS config.
  # The bare-minimum extras (fileSystems, bootloader, stateVersion)
  # exist solely to satisfy NixOS's required-option assertions; we
  # don't read anything from them.
  testEval = nixpkgs.lib.nixosSystem {
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

          rendering = {
            backend               = "nvidia";
            nvidiaPackage         = stubNvidiaPackage;
            extraInitrdStorePaths = stubExtraStorePaths;
          };

          greeterCommand = "${pkgs.writeShellScript "halmasuit-test-greeter" ''
            exec ${pkgs.coreutils}/bin/sleep infinity
          ''}";
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

  cfg = testEval.config;
  svc = cfg.boot.initrd.systemd.services.halmasuit;

  # The initramfs closure ROOTS. Each entry is a submodule attrset
  # `{ source, target, enable, dlopen }` — `.source` is the /nix/store
  # path that NixOS follows transitively into the cpio. coreutils +
  # gnugrep flow in via the nvidia-devnodes script's own references,
  # not as direct entries.
  initrdStorePaths = map (e: e.source) cfg.boot.initrd.systemd.storePaths;

  # Service definition fields — we assert against the structured
  # values that NixOS will use to render the unit, not the rendered
  # text itself (which has `text = null` when the unit comes from
  # services.X; the rendered output lives in a sibling derivation).
  serviceEnv         = svc.environment;
  serviceExecPre     = builtins.toString svc.serviceConfig.ExecStartPre;

  # Stringify env for grep — `Environment=KEY=value` is the unit-file
  # shape NixOS will emit, but the structured `environment` attrset
  # is the source of truth.
  envFlat = builtins.concatStringsSep "\n"
    (pkgs.lib.mapAttrsToList (k: v: "${k}=${toString v}") serviceEnv);
in
pkgs.runCommand "halmasuit-nvidia-egl-env-gate" { } ''
  set -eu

  mkdir -p $out

  cat > $out/service.env <<'EOF'
${envFlat}
EOF

  cat > $out/exec-start-pre.txt <<'EOF'
${serviceExecPre}
EOF

  cat > $out/initrd-store-paths.txt <<'EOF'
${builtins.concatStringsSep "\n" initrdStorePaths}
EOF

  fail() {
    echo "──── halmasuit.service environment ──"
    cat $out/service.env
    echo "──── halmasuit.service ExecStartPre ─"
    cat $out/exec-start-pre.txt
    echo "──── initramfs storePaths ───────────"
    cat $out/initrd-store-paths.txt
    echo "─────────────────────────────────────"
    echo "FAIL: $1"
    exit 1
  }

  # ── (1) Env: __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS ─────────────────
  expected='__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=${expectedConfigDirs}'
  grep -qF "$expected" $out/service.env \
    || fail "missing env line: $expected"

  # ── (2) Sibling env: NVIDIA glvnd ICD pointer ────────────────────
  grep -qF '__EGL_VENDOR_LIBRARY_FILENAMES=' $out/service.env \
    || fail "__EGL_VENDOR_LIBRARY_FILENAMES env missing"
  grep -qF '__GLX_VENDOR_LIBRARY_NAME=nvidia' $out/service.env \
    || fail '__GLX_VENDOR_LIBRARY_NAME=nvidia missing'

  # ── (3) Descriptor dirs actually contain *.json plugins ──────────
  for d in $(echo '${expectedConfigDirs}' | tr ':' ' '); do
    [ -n "$(ls $d/*.json 2>/dev/null)" ] \
      || fail "plugin descriptor dir $d has no *.json files"
  done

  # ── (4) ExecStartPre includes the nvidia-devnodes script ─────────
  grep -qE 'halmasuit-nvidia-devnodes' $out/exec-start-pre.txt \
    || fail "ExecStartPre missing halmasuit-nvidia-devnodes"

  # ── (5) The devnodes script IS in initramfs storePaths ───────────
  # THE gen-383 regression gate. NixOS's initrd-builder follows
  # transitive closures of storePaths roots — it does NOT scan unit
  # text for additional refs. A writeShellScript referenced from a
  # unit but absent from storePaths lands in the unit file but not
  # in the cpio, and systemd dies with status=203/EXEC.
  grep -qE 'halmasuit-nvidia-devnodes' $out/initrd-store-paths.txt \
    || fail "halmasuit-nvidia-devnodes not in boot.initrd.systemd.storePaths"

  echo "halmasuit-nvidia-egl-env-gate: PASS"
''
