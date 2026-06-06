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
    VBLANK = "${pkgs.python3}/bin/python3 ${./lib/nvidia-vblank-probe.py}"

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

    # Is the GPU actually scanning out (real vblanks)? Decides whether a
    # 0x0 CRC is a patch-timing issue (vblanks live) or a no-sink issue
    # (vblanks frozen → need a monitor/dummy plug on the GPU).
    print("=== DRM vblank rate per pipe ===\n" + machine.succeed(f"{VBLANK} {card}"))

    # Which connectors halmasuit lit (so the human knows which port).
    lit = machine.succeed(
        "journalctl -u halmasuit -o cat | grep -oE 'connector bound to dedicated CRTC\",\"connector\":\"[A-Z0-9-]+' "
        "| grep -oE '[A-Z]+-[A-Z0-9-]+' | sort -u || true"
    )
    print("=== connectors halmasuit lit ===\n" + lit)

    # Root-cause the black scanout (gambit:debugging): the eyeball already
    # confirmed BLACK on DP-2/DP-3. Dump the live DRM atomic state to see
    # whether a real content FB (size/format/MODIFIER) is on the primary
    # scanout plane, and any nvidia flip/atomic errors. Prime suspect:
    # a scanout buffer modifier the NVIDIA display engine accepts at
    # modeset but cannot actually scan out (drm.rs:79/968 format negotiation).
    # Confirmation watch (Epic #45 rung-4): with 8-bit scanout forced,
    # does the physical monitor now show solid gray (vs black on 10-bit)?
    import time as _t
    print("=== WATCH THE PHYSICAL MONITOR NOW for ~30s — expect SOLID MID-GRAY, rock-steady ===")
    _t.sleep(30)
    print("=== watch window done ===")

    bdf = "0000:00:09.0"  # the passed-through 5070 Ti (drmDevice pin)
    print("=== DRM atomic state (planes/fbs/format/modifier/crtc) ===\n" +
          machine.execute(f"cat /sys/kernel/debug/dri/{bdf}/state 2>&1 | head -120")[1])
    print("=== framebuffers ===\n" +
          machine.execute(f"cat /sys/kernel/debug/dri/{bdf}/framebuffer 2>&1 | head -40")[1])
    print("=== halmasuit journal: render/flip/modifier/format ===\n" +
          machine.execute("journalctl -u halmasuit -o cat | grep -iE 'modifier|format|plane|flip|scanout|queue|present|render_frame|primary' | tail -40 || true")[1])
    print("=== dmesg: nvidia/drm/flip/atomic/fault ===\n" +
          machine.execute("dmesg | grep -iE 'nvidia|nvrm|drm|modifier|flip|atomic|fault|EVO|head' | tail -50 || true")[1])

    # Read the post-NVIDIA hardware scanout CRC for ~3s per crtc, all
    # three taps. DIAGNOSTIC pass: print everything so we can see which
    # tap carries a real, content-sensitive value (anti-tautology: a
    # tap that is always 0x0 regardless of content proves nothing).
    polled = {}
    for cid in crtcs:
        out = machine.succeed(f"{POLL} {cid} 3 {card}")
        print(f"=== crtc {cid} CRC poll ===\n{out}")
        # The poller emits space-separated `key=val` tokens; tokenize on
        # whitespace (NOT first-`=`-per-line, which breaks multi-pair
        # lines like `comp_supported=1 comp_distinct=1 comp_values=0x...`).
        fields = {}
        for line in out.splitlines():
            for tok in line.split():
                if "=" in tok:
                    k, v = tok.split("=", 1)
                    fields[k] = v
        polled[cid] = fields
        assert "ioctl_err" not in fields, f"crtc {cid}: ioctl error {fields.get('ioctl_err')}"
        # The vblank-wait patch slows each ioctl to ~one frame, so ~30
        # samples in 3s is expected (was ~90 with the no-op one-shot).
        assert int(fields.get("samples", "0")) >= 10, f"crtc {cid}: too few samples"

    for tap in ("comp", "rg", "out"):
        for cid in crtcs:
            p = polled[cid]
            print(f"TAP {tap} crtc {cid}: distinct={p.get(f'{tap}_distinct')} "
                  f"values={p.get(f'{tap}_values')}")

    # === THE ANTI-TAUTOLOGY GATE (Epic #45 req 4) ===
    # Lock onto compositorCrc32: pre-dither, so a CONSTANT color yields
    # exactly ONE hardware CRC per head WHEN the pipeline genuinely scans
    # it out. (The rg/out taps legitimately vary per frame from dithering,
    # so they are NOT used for the no-flicker check.)
    comp_crc = {}
    for cid in crtcs:
        p = polled[cid]
        vals = p.get("comp_values", "")
        distinct = int(p.get("comp_distinct", "0"))
        # 1. NON-ZERO: 0x0 = driver dropped the frame / no real scanout —
        #    the exact "rendered then dropped on the floor" tautology this
        #    gate exists to catch (req 4).
        assert vals and vals != "0x0", (
            f"crtc {cid}: compositor CRC is {vals!r} — scanout produced no "
            f"real content (driver-drop / black scanout)."
        )
        # 2. NO FLICKER: a static color must hash to exactly one value over
        #    the sampling window.
        assert distinct == 1, (
            f"crtc {cid}: FLICKER — compositor CRC varied ({distinct} "
            f"distinct: {vals}); a constant color must be scanout-stable."
        )
        comp_crc[cid] = vals

    # 3. CONTENT-SENSITIVE (anti-tautology): the two DIFFERENT-resolution
    #    heads must produce DIFFERENT compositor CRCs. Identical values
    #    would mean the CRC is content-independent and proves nothing.
    if len(crtcs) >= 2:
        assert len(set(comp_crc.values())) == len(crtcs), (
            f"anti-tautology FAIL: heads share a compositor CRC ({comp_crc}) "
            f"— the CRC is not tracking scanned content."
        )

    print(f"visual-nvidia-flicker: PASS — flicker-free scanout, "
          f"content-sensitive compositor CRCs {comp_crc}")

    # Graceful GPU teardown so the next run works without a host reboot
    # (Blackwell reset wedge — see tests/lib/nvidia-teardown.sh).
    print(machine.execute("sh ${./lib/nvidia-teardown.sh}")[1])
    machine.shutdown()
  '';
}
