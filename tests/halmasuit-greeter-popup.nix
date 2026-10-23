# tests/halmasuit-greeter-popup.nix — a greeter's popup is composited.
#
# DMS's greeter opens its dropdown lists (the session picker) as
# xdg_popups parented to its Overlay layer surface. The greeter here is
# a keyboard-interactive Overlay layer-shell client that opens one
# magenta xdg_popup below-right of a point on its surface. The test
# reads halmasuit's own composited frame (Snapshot) and asserts the
# popup's centre pixel is magenta, not the parent surface's green, then
# moves an absolute pointer onto the popup and clicks: the pointer
# enter and the button press must land on the popup, not the layer
# surface beneath it.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  halmasuit-layer-shell-test-client,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-greeter-popup";

  skipTypeCheck = true;

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
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
        wallpaper       = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand = "${pkgs.writeShellScript "halmasuit-popup-greeter" ''
          export HALMASUIT_TESTCLIENT_KEYBOARD=1
          export HALMASUIT_TESTCLIENT_LAYER=overlay
          export HALMASUIT_TESTCLIENT_POPUP=1
          exec ${halmasuit-layer-shell-test-client}/bin/halmasuit-layer-shell-test-client
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

      # /dev/uinput for the test's absolute pointer.
      boot.kernelModules = [ "uinput" ];

      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 2048;
        cores      = 2;
        diskSize   = 2048;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import re
    import struct
    import sys
    import zlib

    sys.path.insert(0, "${./lib}")
    import visual

    MAGENTA = (0xFF, 0x00, 0xFF, 0xFF)

    def png_pixel(path, x, y):
        # halmasuit's Snapshot PNG: 8-bit RGBA. Undo the row filters
        # (PNG spec section 9) down to row y.
        data = path.read_bytes()
        pos, width, idat = 8, None, b""
        while pos < len(data):
            length, kind = struct.unpack(">I4s", data[pos:pos + 8])
            body = data[pos + 8:pos + 8 + length]
            pos += 12 + length
            if kind == b"IHDR":
                width, _, depth, color = struct.unpack(">IIBB", body[:10])
                assert (depth, color) == (8, 6), f"not 8-bit RGBA: {depth}, {color}"
            elif kind == b"IDAT":
                idat += body
        raw = zlib.decompress(idat)
        bpp, stride = 4, width * 4
        prev = bytearray(stride)
        for row_index in range(y + 1):
            start = row_index * (stride + 1)
            kind, line = raw[start], bytearray(raw[start + 1:start + 1 + stride])
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                if kind == 1:
                    line[i] = (line[i] + a) & 0xFF
                elif kind == 2:
                    line[i] = (line[i] + b) & 0xFF
                elif kind == 3:
                    line[i] = (line[i] + (a + b) // 2) & 0xFF
                elif kind == 4:
                    p0 = a + b - c
                    pa, pb, pc = abs(p0 - a), abs(p0 - b), abs(p0 - c)
                    pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                    line[i] = (line[i] + pred) & 0xFF
                else:
                    assert kind == 0, f"row {row_index}: unknown PNG filter {kind}"
            prev = line
        return tuple(prev[x * 4:x * 4 + 4])

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")
    machine.wait_until_succeeds("busctl --system status org.halmasuit", timeout=30)
    machine.wait_until_succeeds(
        "journalctl -b -o cat | grep -qF 'layer-shell-test-client: popup painted'",
        timeout=60,
    )
    line = machine.succeed(
        "journalctl -b -o cat | grep -F 'layer-shell-test-client: popup painted' | tail -1"
    )
    m = re.search(r"x=(-?\d+) y=(-?\d+) w=(\d+) h=(\d+)", line)
    assert m, f"no popup geometry in {line!r}"
    x, y, w, h = (int(v) for v in m.groups())
    print(f"popup at {x},{y} size {w}x{h}")

    # The parent layer surface is fullscreen at the output origin, so
    # popup coordinates are output coordinates. Poll: the popup's
    # buffer lands a frame after its configure.
    cx, cy = x + w // 2, y + h // 2
    pixel = None
    for attempt in range(20):
        pixel = png_pixel(visual.capture(machine, f"popup-{attempt}"), cx, cy)
        if pixel == MAGENTA:
            break
        machine.sleep(0.5)
    assert pixel == MAGENTA, f"popup centre ({cx},{cy}) is {pixel}, not the popup's magenta"

    # An absolute uinput pointer (halmasuit's input hotplug adds it)
    # moves onto the popup's centre and clicks.
    machine.succeed(
        "${pkgs.python3}/bin/python3 -I ${./lib/uinput_pointer.py} "
        f"1280 800 {cx} {cy} click"
    )
    machine.wait_until_succeeds(
        "journalctl -b -o cat | grep -qF 'layer-shell-test-client: pointer enter popup'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -b -o cat | grep -qF "
        "'layer-shell-test-client: pointer press popup button=272'",
        timeout=30,
    )
    presses = machine.succeed(
        "journalctl -b -o cat | grep -F 'layer-shell-test-client: pointer press' || true"
    ).splitlines()
    assert all(" popup " in p for p in presses), f"a press missed the popup: {presses}"

    print("halmasuit-greeter-popup: ALL ASSERTIONS PASSED")
  '';
}
