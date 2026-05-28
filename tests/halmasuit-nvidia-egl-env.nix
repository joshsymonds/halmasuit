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

  # ── (6) Every text-format storePaths entry's /nix/store refs are
  #       ALSO in storePaths ────────────────────────────────────────
  # THE gen-384 regression gate. make-initrd-ng walks runtime deps
  # via ELF RUNPATH/NEEDED only — for text files (shell scripts,
  # config files, etc.), /nix/store references inside the body are
  # NOT followed. So every binary a script INVOKES via absolute path
  # must be listed as its OWN storePaths entry. The gen-384 failure
  # mode: the devnodes script was in storePaths, but its bare
  # gnugrep ref was not, and systemd's ExecStartPre crashed with
  # "/nix/store/.../gnugrep-3.12/bin/grep: No such file or directory".
  #
  # The check: extract every /nix/store/HASH-NAME prefix referenced
  # by any text-format storePaths entry, then for each one verify
  # SOME storePaths entry has that prefix (the binary must be
  # reachable as a storePaths entry; make-initrd-ng will resolve
  # the rest via the directory walk).
  #
  # Tools: file(1) to classify (skip ELF — those follow runpath),
  # grep -oP to extract refs.
  echo "── closure check: text-format storePaths entries ─────────"
  any_failed=0
  while IFS= read -r entry; do
    [ -z "$entry" ] && continue
    if [ ! -e "$entry" ]; then continue; fi
    # ELF: make-initrd-ng follows RUNPATH, skip the text-scan check.
    if ${pkgs.file}/bin/file -bL "$entry" 2>/dev/null | grep -q ELF; then
      continue
    fi
    # Some "entries" are directories (e.g. a package root). For
    # those, make-initrd-ng walks the contents — each file gets
    # its own pass through copy_file, ELF deps are followed. We
    # only need to scan PLAIN-TEXT files at storePaths leaves.
    if [ -d "$entry" ]; then continue; fi
    if ! ${pkgs.file}/bin/file -bL "$entry" 2>/dev/null | grep -qiE 'text|script'; then
      continue
    fi
    refs=$(${pkgs.gnugrep}/bin/grep -oP '/nix/store/[a-z0-9]{32}-[^/[:space:]"'"'"']+' "$entry" 2>/dev/null | sort -u || true)
    [ -z "$refs" ] && continue
    while IFS= read -r ref; do
      [ -z "$ref" ] && continue
      # Extract the store-prefix (everything up to the second `/`
      # under /nix/store/). A ref like
      # `/nix/store/HASH-coreutils-9.10/bin/cut` reduces to
      # `/nix/store/HASH-coreutils-9.10`.
      prefix=$(echo "$ref" | ${pkgs.gnugrep}/bin/grep -oP '^/nix/store/[a-z0-9]{32}-[^/]+')
      # Does ANY storePaths entry have this prefix?
      if ! ${pkgs.gnugrep}/bin/grep -qxF "$prefix" $out/initrd-store-paths.txt \
        && ! ${pkgs.gnugrep}/bin/grep -q "^$prefix/" $out/initrd-store-paths.txt; then
        echo "  MISS: $entry refs $ref"
        echo "        but $prefix not in storePaths"
        any_failed=1
      fi
    done <<EOF
$refs
EOF
  done < $out/initrd-store-paths.txt
  if [ "$any_failed" -ne 0 ]; then
    fail "text-format storePaths entries reference paths not in storePaths"
  fi

  echo "halmasuit-nvidia-egl-env-gate: PASS"
''
