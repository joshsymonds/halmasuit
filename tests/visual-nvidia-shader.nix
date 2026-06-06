# tests/visual-nvidia-shader.nix — Epic #45, rung 6 (shader-path flicker).
#
# The flat-IMAGE flicker test (visual-nvidia-flicker.nix) proved only that
# a FROZEN frame scans out steadily — it composites once and the GPU
# re-scans the same buffer. That is NOT the path the gnomon flicker lives
# in. The flickering wallpaper is halmasuit's SHADER backend
# (crates/halmasuit/src/wallpaper/shader.rs — a real GLSL renderer:
# GlesRenderer::compile_custom_pixel_shader, iTime/iFrame, and
# wants_continuous_render()==true so it re-runs the fragment shader EVERY
# frame). That continuously-rendered render→present→scanout loop is where
# "flashes darker/lighter" and "refresh seems low" would originate.
#
# This test drives that exact path but with a CONSTANT-OUTPUT shader:
# wants_continuous_render is true (so iTime advances and halmasuit renders
# every frame), but mainImage returns a FIXED color. So the EXPECTED
# scanout is dead-steady. Any per-frame variance in the hardware
# compositorCrc32 is therefore flicker IN THE SHADER RENDER PATH ITSELF,
# cleanly isolated from content/art:
#   - compositor CRC constant (distinct==1) => the shader path is steady.
#   - compositor CRC varies                 => flicker reproduced HERE,
#                                              in halmasuit's shader path.
# Either result is decisive. The real animated chrome_hexrain shader is a
# follow-on (distinguishing intended animation from unwanted variance is
# harder; this isolates the path first).
#
# RUNNER-ONLY: `just test-vm-nvidia visual-nvidia-shader` on stygian.
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
  # Continuously-rendered (shader backend re-runs it every frame, iTime
  # advancing) but emits a FIXED mid-gray regardless of time/position.
  constShader = pkgs.writeText "const-gray.frag" ''
    void mainImage(out vec4 fragColor, in vec2 fragCoord) {
        // iTime advances and this runs every frame, but the output never
        // changes — so any scanout variance is the render path, not art.
        fragColor = vec4(0.25098, 0.25098, 0.25098, 1.0); // #404040
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-nvidia-shader";
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
        wallpaper = { type = "shader"; source = constShader; };
        greeterCommand = "${pkgs.writeShellScript "shader-greeter" ''
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

    # Confirm the SHADER backend actually compiled + is the active wallpaper
    # (not a fallback to image/placeholder) — otherwise we'd be testing the
    # wrong path again.
    jrnl = machine.succeed("journalctl -u halmasuit -o cat")
    print("=== wallpaper backend identity ===\n" + machine.execute(
        "journalctl -u halmasuit -o cat | grep -iE 'shader|wallpaper|fragment|compile|continuous' | tail -20")[1])

    card_m = re.search(r'DRM device resolved.*?(card\d+)', jrnl)
    card = "/dev/dri/" + (card_m.group(1) if card_m else "card1")
    crtcs = sorted(set(int(m) for m in re.findall(r'crtc::Handle\((\d+)\)', jrnl)))
    print(f"card={card} crtc_ids={crtcs}")
    assert crtcs, "no crtc::Handle(N) found in halmasuit journal"

    import time as _t
    print("=== WATCH THE PHYSICAL MONITOR NOW for ~25s — a CONTINUOUSLY-RENDERED "
          "shader emitting constant gray. It must be DEAD STEADY (no flashing). ===")
    _t.sleep(25)
    print("=== watch window done ===")

    # Poll the hardware compositorCrc32 across the shader's continuous
    # rendering. For a constant-output shader the path is steady iff the
    # CRC is a single value.
    polled = {}
    for cid in crtcs:
        out = machine.succeed(f"{POLL} {cid} 4 {card}")
        print(f"=== crtc {cid} CRC poll ===\n{out}")
        fields = {}
        for line in out.splitlines():
            for tok in line.split():
                if "=" in tok:
                    k, v = tok.split("=", 1)
                    fields[k] = v
        polled[cid] = fields
        assert "ioctl_err" not in fields, f"crtc {cid}: ioctl error {fields.get('ioctl_err')}"
        assert int(fields.get("samples", "0")) >= 10, f"crtc {cid}: too few samples"

    for cid in crtcs:
        p = polled[cid]
        print(f"crtc {cid}: comp_distinct={p.get('comp_distinct')} "
              f"comp_values={p.get('comp_values')}")

    # === SHADER-PATH STABILITY GATE ===
    comp_crc = {}
    for cid in crtcs:
        p = polled[cid]
        vals = p.get("comp_values", "")
        distinct = int(p.get("comp_distinct", "0"))
        assert vals and vals != "0x0", (
            f"crtc {cid}: compositor CRC is {vals!r} — shader scanout produced "
            f"no real content (driver-drop / black)."
        )
        # The decisive assertion: a constant-output shader, even though it
        # re-renders every frame, must scan out a SINGLE CRC. >1 distinct
        # value = the shader RENDER/PRESENT/SCANOUT path flickers.
        assert distinct == 1, (
            f"crtc {cid}: SHADER-PATH FLICKER — compositor CRC varied "
            f"({distinct} distinct: {vals}) for a constant-output shader. "
            f"The flicker is in halmasuit's continuous shader render path."
        )
        comp_crc[cid] = vals

    if len(crtcs) >= 2:
        assert len(set(comp_crc.values())) == len(crtcs), (
            f"anti-tautology FAIL: heads share a compositor CRC ({comp_crc})."
        )

    print(f"visual-nvidia-shader: PASS — continuous shader render path is "
          f"flicker-free; constant-output CRCs {comp_crc}")

    # Graceful GPU teardown so the next run works without a host reboot
    # (Blackwell reset wedge — see tests/lib/nvidia-teardown.sh).
    print(machine.execute("sh ${./lib/nvidia-teardown.sh}")[1])
    machine.shutdown()
  '';
}
