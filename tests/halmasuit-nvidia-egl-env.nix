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

          # Epic #42 R4: assert the diagnostic trace option wires onto
          # the broker's systemd Environment= when enabled.
          diagnostic.brokerTraceFrames = true;
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

  # Epic #42 R2: the privileged broker unit ALSO needs the NVIDIA EGL
  # env, because the broker is the process that fork-then-execve's the
  # greeter with an EXPLICIT envv. The compositor's env doesn't
  # propagate to the greeter — `worker::greeter_child_env_pairs`
  # harvests from the broker's own env. Pre-gen-405 this was the bug:
  # halmasuit.service had the env, halmasuit-session.service didn't,
  # and the greeter's libEGL fell through to MESA-LOADER.
  brokerSvc = cfg.systemd.services."halmasuit-session";
  brokerEnv = brokerSvc.environment;
  brokerEnvFlat = builtins.concatStringsSep "\n"
    (pkgs.lib.mapAttrsToList (k: v: "${k}=${toString v}") brokerEnv);

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

  # Pull the nvidia-devnodes script out of ExecStartPre — used by
  # the script-runtime simulation in check #7 below.
  devnodesScriptPath = pkgs.lib.findFirst
    (s: pkgs.lib.hasInfix "halmasuit-nvidia-devnodes" (toString s))
    null
    svc.serviceConfig.ExecStartPre;

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

  cat > $out/broker.env <<'EOF'
${brokerEnvFlat}
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

  # ── (2b) Epic #42 R2: SAME env on the privileged broker unit ─────
  # The broker forks-then-execve's the greeter with an explicit envv
  # (worker::greeter_child_env_pairs). It harvests its OWN env to
  # bridge NVIDIA EGL into the greeter's envv — that bridge only
  # works if the keys are present on this unit. Pre-gen-405, this
  # block was on halmasuit.service but missing from
  # halmasuit-session.service, and the greeter logged
  # "libEGL warning: failed to get driver name for fd -1" because
  # NVIDIA's ICD wasn't discoverable in its env. This gate fails fast
  # on a future regression of the same shape.
  echo "── broker.env ──────────────────────────────────────────────"
  cat $out/broker.env
  echo "────────────────────────────────────────────────────────────"
  grep -qF '__EGL_VENDOR_LIBRARY_FILENAMES=' $out/broker.env \
    || fail "Epic #42 R2: __EGL_VENDOR_LIBRARY_FILENAMES missing on halmasuit-session.service env"
  grep -qF '__GLX_VENDOR_LIBRARY_NAME=nvidia' $out/broker.env \
    || fail "Epic #42 R2: __GLX_VENDOR_LIBRARY_NAME=nvidia missing on halmasuit-session.service env"
  grep -qF '__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=' $out/broker.env \
    || fail "Epic #42 R2: __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS missing on halmasuit-session.service env"

  # ── (2c) Epic #42 R4: brokerTraceFrames option wires Environment= ─
  # The option `services.halmasuit.diagnostic.brokerTraceFrames` is
  # set to `true` in the test config above; the unit's Environment=
  # MUST therefore carry `HALMASUIT_BROKER_TRACE_FRAMES=1` so the
  # broker's `wire_trace` module reads it at startup and turns on
  # frame logging. Defends against a future refactor that renames
  # the env key, or moves the wire into a drop-in that doesn't get
  # rendered, etc.
  grep -qF 'HALMASUIT_BROKER_TRACE_FRAMES=1' $out/broker.env \
    || fail "Epic #42 R4: HALMASUIT_BROKER_TRACE_FRAMES=1 missing on halmasuit-session.service env (option wired but not propagated)"

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

  # ── (7) Script-runtime simulation: nvidia-devnodes against
  #       a fixture /proc tree ─────────────────────────────────────
  # The gen-386 regression gate. Closure checks (#5, #6) verify the
  # script's binary deps are present, but they say nothing about
  # whether the script's LOGIC actually creates the expected /dev
  # nodes. Gen 386 shipped a buggy `grep '^Minor'` pattern that
  # silently matched nothing on the real /proc/driver/nvidia/gpus/*/
  # information format (which has "Device Minor:" — the line starts
  # with "Device"). Script succeeded with set -e, /dev/nvidia0 never
  # got mknod'd, libEGL_nvidia died at GBM platform registration.
  #
  # Strategy: set up a minimal fake /proc tree mimicking the kernel's
  # output format, sed-substitute the script's absolute mknod into
  # `echo` and the /dev + /proc paths into the fixture dirs, then
  # assert the expected mknod-calls fire by grepping stdout.
  echo "── script-runtime simulation ─────────────────────────────"
  fixture=$TMPDIR/devnode-fixture
  mkdir -p $fixture/proc/driver/nvidia/gpus/0/ $fixture/dev
  printf 'Model:         NVIDIA TestGPU\nDevice Minor: \t 0\n' \
    > $fixture/proc/driver/nvidia/gpus/0/information
  printf 'Character devices:\n  1 mem\n235 nvidia-uvm\n' \
    > $fixture/proc/devices

  # Patch the script: substitute mknod → echo, /proc/ → fixture,
  # /dev/ → fixture. The `[ -e /dev/X ]` guards now check fixture
  # paths (empty dir) so mknod IS called every time. The grep/cut/tr
  # invocations stay intact so we exercise the real text-parsing.
  patched=$TMPDIR/script-patched.sh
  ${pkgs.gnused}/bin/sed \
    -e "s|/nix/store/[^/]*-coreutils[^/]*/bin/mknod|echo MKNOD|g" \
    -e "s|/proc/driver|$fixture/proc/driver|g" \
    -e "s|/proc/devices|$fixture/proc/devices|g" \
    -e "s|/dev/nvidia|$fixture/dev/nvidia|g" \
    "${devnodesScriptPath}" > $patched
  chmod +x $patched

  echo "── patched script output ────────────────────────"
  set +e
  sim_out=$(${pkgs.bash}/bin/bash $patched 2>&1)
  sim_rc=$?
  set -e
  echo "$sim_out"
  [ $sim_rc -eq 0 ] || fail "script returned non-zero rc=$sim_rc"

  echo "── verifying expected mknod calls ────────────────"
  echo "$sim_out" | ${pkgs.gnugrep}/bin/grep -qE 'MKNOD.*nvidiactl c 195 255' \
    || fail "no mknod for /dev/nvidiactl"
  echo "$sim_out" | ${pkgs.gnugrep}/bin/grep -qE 'MKNOD.*nvidia0 c 195 0' \
    || fail "no mknod for /dev/nvidia0 (regex broke?)"
  echo "$sim_out" | ${pkgs.gnugrep}/bin/grep -qE 'MKNOD.*nvidia-modeset c 195 254' \
    || fail "no mknod for /dev/nvidia-modeset"
  echo "$sim_out" | ${pkgs.gnugrep}/bin/grep -qE 'MKNOD.*nvidia-uvm c 235 0' \
    || fail "no mknod for /dev/nvidia-uvm (uvm major regex broke?)"
  echo "$sim_out" | ${pkgs.gnugrep}/bin/grep -qE 'MKNOD.*nvidia-uvm-tools c 235 1' \
    || fail "no mknod for /dev/nvidia-uvm-tools"

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
