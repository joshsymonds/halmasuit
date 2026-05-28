# tests/halmasuit-nvidia-egl-env.nix — regression gate for the
# `__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS` env var on the `nvidia`
# rendering backend.
#
# Why this exists: we cannot replicate the actual NVIDIA-EGL failure
# in CI (consumer Blackwell + single GPU has no passthrough route to a
# guest VM; the headless test substrate is virtio-gpu-pci). What we
# CAN catch deterministically is the SHAPE of the unit env: the
# bug that bit gnomon on 2026-05-28 was a missing env var, and that
# shape is observable at Nix-eval / boot time without ever asking
# NVIDIA to render a frame.
#
# This test:
#   1. Boots NixOS with `services.halmasuit.fromInitrd.enable = true`
#      and `rendering.backend = "nvidia"`, supplying a stub
#      `nvidiaPackage` and a stub `extraInitrdStorePaths` set so the
#      module eval succeeds without a real NVIDIA closure in the host
#      nixpkgs.
#   2. Reads the initramfs halmasuit.service unit file (the one that
#      runs on gnomon — Phase B) and asserts the env var is present
#      AND its value is the colon-separated `share/egl/
#      egl_external_platform.d/` path for the supplied extras.
#   3. Reads the rootfs halmasuit.service unit (when `enable = true`
#      instead of `fromInitrd.enable`) and asserts the same.
#
# This test does NOT start halmasuit — the VM substrate isn't NVIDIA,
# so a runtime start would either fail (the original gnomon symptom)
# or succeed spuriously on virtio-gpu's EGL Mesa path. The goal is a
# pure unit-shape probe: did the module emit the right env vars?

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # Stand-ins for the real NVIDIA closure. Mesa has the right
  # share/glvnd/egl_vendor.d shape to make the module eval pass; we
  # only care about the OUTGOING env vars. We assert the env's VALUE
  # is the search-path constructed from the extras list, so we don't
  # need real plugin packages.
  stubNvidiaPackage     = pkgs.mesa;
  stubExtraStorePaths   = [ pkgs.egl-wayland pkgs.egl-gbm ];

  # Pre-compute the expected search path so the testScript can
  # interpolate it. Order matters and matches lib.makeSearchPath.
  expectedConfigDirs = pkgs.lib.makeSearchPath
    "share/egl/egl_external_platform.d"
    stubExtraStorePaths;
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-nvidia-egl-env";

  nodes.machine =
    { ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        fromInitrd.enable = true;
        package           = halmasuit;
        session.package   = halmasuit-session;
        greeterUid        = 999;
        greeterGroup      = "halmasuit-greeter";
        compositorUid     = 998;

        # The thing under test: the nvidia branch + extras.
        rendering = {
          backend               = "nvidia";
          nvidiaPackage         = stubNvidiaPackage;
          extraInitrdStorePaths = stubExtraStorePaths;
        };

        # No-op greeter — never executed in this test, but the
        # module assertion requires a value.
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

      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # The initramfs unit (Phase B path — gnomon's deployment shape).
    # NixOS bakes initramfs units into a known store path; we resolve
    # via the kernel-modules tree's neighbour.
    initrd_unit_path = machine.succeed(
        "find /nix/store -maxdepth 3 -name halmasuit.service "
        "-path '*initrd-units*' | head -1"
    ).strip()
    assert initrd_unit_path, "could not locate initramfs halmasuit.service"

    initrd_unit = machine.succeed(f"cat {initrd_unit_path}")

    # ─ Assert __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS is set ────────────
    # The exact env directive shape systemd emits is
    #   Environment="__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=…"
    expected_env_line = (
        'Environment="__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS='
        '${expectedConfigDirs}"'
    )
    assert expected_env_line in initrd_unit, (
        f"initramfs halmasuit.service is missing the expected env line.\n"
        f"  expected: {expected_env_line!r}\n"
        f"  unit text was:\n{initrd_unit}"
    )

    # ─ Assert the descriptor dirs actually contain *.json plugins ──
    # Defensive: a misconfigured extraInitrdStorePaths could yield a
    # well-formed env value pointing at empty dirs. Probe each path.
    for dir_path in "${expectedConfigDirs}".split(":"):
        listing = machine.succeed(f"ls {dir_path}/*.json 2>/dev/null || true")
        assert listing.strip(), (
            f"plugin descriptor dir {dir_path!r} contains no *.json files; "
            f"extras must ship share/egl/egl_external_platform.d/*.json"
        )

    # ─ Substrate sanity: __EGL_VENDOR_LIBRARY_FILENAMES is the
    #   sibling env that points at the NVIDIA ICD glvnd manifest.
    #   Asserting it stays present so a refactor that drops one env
    #   can't quietly drop the other.
    assert '__EGL_VENDOR_LIBRARY_FILENAMES' in initrd_unit, (
        "the NVIDIA glvnd ICD manifest env is missing — both EGL envs "
        "must travel together"
    )
    assert '__GLX_VENDOR_LIBRARY_NAME="nvidia"' in initrd_unit, (
        "__GLX_VENDOR_LIBRARY_NAME=nvidia missing from initramfs unit"
    )

    # ─ Assert the nvidia-devnodes ExecStartPre is wired ─────────────
    # The nvidia branch's mknod helper script runs before halmasuit's
    # main process to pre-create /dev/nvidiactl + /dev/nvidia* in
    # initramfs (the gnomon 2026-05-28 gen-382 failure mode: nvidia
    # kernel modules don't auto-create those nodes, NixOS udev rules
    # are rootfs-only and PATH-dependent, libEGL_nvidia silently
    # skips its EGL platform registration without them).
    import re
    devnodes_re = re.compile(
        r'ExecStartPre=(\S*halmasuit-nvidia-devnodes\S*)'
    )
    m = devnodes_re.search(initrd_unit)
    assert m, (
        "initramfs halmasuit.service is missing the nvidia-devnodes "
        f"ExecStartPre line. Unit text:\n{initrd_unit}"
    )
    devnodes_script = m.group(1)

    # ─ Assert the devnodes script is actually IN the initramfs ─────
    # This is the gen-383 regression gate. systemd died there with
    # status=203/EXEC because the unit referenced a /nix/store path
    # that wasn't baked into the initramfs closure (NixOS's
    # initrd-builder follows storePaths' transitive closure, NOT
    # arbitrary references discovered by scanning unit text). The
    # check: extract the initramfs cpio, list its contents, verify
    # the script's store path appears.
    initrd_file = machine.succeed(
        "readlink -f /run/booted-system/initrd"
    ).strip()

    # NixOS systemd initramfs is two concatenated cpios: a tiny
    # microcode cpio (~300 KB) followed by a zstd-compressed cpio
    # carrying the actual rootfs. The zstd magic `28 b5 2f fd`
    # marks the second segment. Find it by scanning, then list.
    listing_cmd = (
        f"python3 -c \"import sys\n"
        f"with open('{initrd_file}', 'rb') as f: data = f.read()\n"
        f"i = data.find(b'\\x28\\xb5\\x2f\\xfd')\n"
        f"sys.stdout.buffer.write(data[i:])\" | "
        f"zstd -dc | cpio -t 2>/dev/null"
    )
    initrd_contents = machine.succeed(listing_cmd)

    # The ExecStartPre value is an absolute /nix/store path; the
    # cpio listing has paths relative to /. Strip the leading slash
    # for the membership check.
    script_in_cpio = devnodes_script.lstrip("/")
    assert script_in_cpio in initrd_contents, (
        f"nvidia-devnodes script {devnodes_script!r} is referenced by "
        f"ExecStartPre but NOT present in the initramfs cpio. This is "
        f"the gen-383 regression: storePaths missing the script.\n"
        f"Initramfs file count: {len(initrd_contents.splitlines())}"
    )

    print("halmasuit-nvidia-egl-env: PASS")
  '';
}
