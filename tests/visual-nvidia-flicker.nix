# tests/visual-nvidia-flicker.nix — Epic #45, rung 4.
#
# The anti-tautology centerpiece (req 4) against the observed gnomon
# symptom "the output randomly flashes darker and lighter". Render a
# CONSTANT solid color and verify, at the TRUE POST-NVIDIA (scanout)
# level — NOT halmasuit's own Snapshot/frame_rendered self-report — that
# every scanned-out frame is identical (zero flicker).
#
# Mechanism (resolved by Epic #45 research): nvidia-drm exposes no KMS
# debugfs CRC, BUT the NVIDIA display engine computes a hardware CRC of
# the actual scanned-out frame per head, and the open driver exposes it
# via the private ioctl DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2. That ioctl
# is DRM_RENDER_ALLOW (no DRM master / auth required) and its handler is
# an unguarded drm_crtc_find — so this test reads the live HW CRC for
# the head halmasuit is driving as a SEPARATE root process, no driver
# patch, no dongle. We use the `compositorCrc32` tap (pre-dither, so
# frame-stable). See tests/lib/nvidia-crc-poll.py.
#
# A flat-color IMAGE wallpaper is the source: truly static content, so
# any scanout variance is the pipeline/driver, not the art.
#
# RUNNER-ONLY: `just test-vm-nvidia visual-nvidia-flicker` on stygian.
{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
}:
let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  flatColor = pkgs.runCommand "flat-gray-1920x1200.png" { } ''
    ${pkgs.imagemagick}/bin/magick -size 1920x1200 xc:'#404040' $out
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-nvidia-flicker";
  skipTypeCheck = true;

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ./lib/nvidia-passthrough.nix
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        rendering = {
          backend = "nvidia";
          extraInitrdStorePaths = [ "${pkgs.egl-wayland}" "${pkgs.egl-gbm}" ];
        };
        drmDevice = "pci:0000:00:09.0";
        wallpaper = { type = "image"; source = flatColor; };
        greeterCommand = "${pkgs.writeShellScript "flicker-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
      };

      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      users.users.halmasuit-greeter = {
        isSystemUser = true; uid = 999; group = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true; uid = 998; group = "halmasuit-greeter";
      };
    };

  testScript = ''
    import re

    POLL = "${pkgs.python3}/bin/python3 ${./lib/nvidia-crc-poll.py}"

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "lspci -nnk -d 10de: | grep -q 'Kernel driver in use: nvidia'", timeout=60
    )
    machine.wait_for_unit("halmasuit.service", timeout=120)
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF scanout_active", timeout=120
    )

    # Which DRM card + KMS crtc IDs is halmasuit driving (it logs them)?
    journal = machine.succeed("journalctl -u halmasuit -o cat")
    card_m = re.search(r'DRM device resolved.*?(card\d+)', journal)
    card = "/dev/dri/" + (card_m.group(1) if card_m else "card1")
    crtcs = sorted(set(int(m) for m in re.findall(r'crtc::Handle\((\d+)\)', journal)))
    print(f"card={card} crtc_ids={crtcs}")
    assert crtcs, "no crtc::Handle(N) found in halmasuit journal"

    # Read the post-NVIDIA hardware scanout CRC for ~3s per crtc, all
    # three taps. DIAGNOSTIC pass: print everything so we can see which
    # tap carries a real, content-sensitive value (anti-tautology: a
    # tap that is always 0x0 regardless of content proves nothing).
    polled = {}
    for cid in crtcs:
        out = machine.succeed(f"{POLL} {cid} 3 {card}")
        print(f"=== crtc {cid} CRC poll ===\n{out}")
        fields = dict(
            line.split("=", 1) for line in out.splitlines() if "=" in line
        )
        polled[cid] = fields
        assert "ioctl_err" not in fields, f"crtc {cid}: ioctl error {fields.get('ioctl_err')}"
        assert int(fields.get("samples", "0")) >= 50, f"crtc {cid}: too few samples"

    # Anti-tautology check across the two DIFFERENT-resolution crtcs: a
    # genuine content CRC must differ between them. Report which taps are
    # supported, stable (distinct==1), non-zero, and crtc-distinguishing.
    for tap in ("comp", "rg", "out"):
        per = {cid: polled[cid] for cid in crtcs}
        sup = {cid: per[cid].get(f"{tap}_supported") for cid in crtcs}
        dist = {cid: per[cid].get(f"{tap}_distinct") for cid in crtcs}
        vals = {cid: per[cid].get(f"{tap}_values") for cid in crtcs}
        print(f"TAP {tap}: supported={sup} distinct={dist} values={vals}")

    print("visual-nvidia-flicker: DIAGNOSTIC — taps dumped above; "
          "assertion will lock onto the content-sensitive stable tap next.")
  '';
}
