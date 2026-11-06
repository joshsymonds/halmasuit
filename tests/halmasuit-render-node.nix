# tests/halmasuit-render-node.nix — the render-node split on a real split
# substrate: GL on a vgem render node, scan-out on vkms.
#
# halmasuit-debug runs with `rendering.renderNode = "/dev/dri/renderD128"`
# (vgem: a render-only node, llvmpipe GLES) and scans out on vkms, the only
# DRM card with a connected connector. No virtio-gpu or VGA device exists,
# so Auto device selection lands on vkms.
#
# Asserts, all live:
#
#   * The journal names the resolved render node, and the output was bound
#     through the CPU-copy presenter (`copy_scanout` true).
#   * The vkms CRTC is active, and the framebuffer it scans out is XRGB8888
#     at the CRTC's mode size.
#   * The pixels actually scanned out on vkms match halmasuit's own
#     `Snapshot()` of the same scene: ssimulacra2 >= EXACT_IMAGE_THRESHOLD.
#     A black or absent scan-out fails here.
#   * Every `frame_rendered` passes `assert_no_flash_stream`.
#
# The scan-out capture is a small C helper built in this file. It finds the
# vkms card by driver name, reads the CRTC's current framebuffer through
# GETFB2, maps the dumb buffer, and writes the packed XRGB8888 rows to a raw
# file. The host converts that to PNG for ssimulacra2.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-session,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs { inherit system; };

  vkmsScanoutCaptureSrc = pkgs.writeText "vkms-scanout-capture.c" ''
    #include <fcntl.h>
    #include <stdint.h>
    #include <stdio.h>
    #include <string.h>
    #include <sys/mman.h>
    #include <unistd.h>

    #include <drm_fourcc.h>
    #include <xf86drm.h>
    #include <xf86drmMode.h>

    static int open_vkms(void) {
        char path[32];
        for (int i = 0; i < 16; i++) {
            snprintf(path, sizeof path, "/dev/dri/card%d", i);
            int fd = open(path, O_RDWR | O_CLOEXEC);
            if (fd < 0) {
                continue;
            }
            drmVersionPtr version = drmGetVersion(fd);
            int is_vkms = version != NULL && strcmp(version->name, "vkms") == 0;
            if (version != NULL) {
                drmFreeVersion(version);
            }
            if (is_vkms) {
                printf("device=%s\n", path);
                return fd;
            }
            close(fd);
        }
        return -1;
    }

    int main(int argc, char **argv) {
        if (argc != 2) {
            fprintf(stderr, "usage: %s <raw-out>\n", argv[0]);
            return 2;
        }

        int fd = open_vkms();
        if (fd < 0) {
            fprintf(stderr, "no vkms DRM device\n");
            return 3;
        }

        drmModeRes *res = drmModeGetResources(fd);
        if (res == NULL) {
            fprintf(stderr, "drmModeGetResources failed\n");
            return 1;
        }
        drmModeCrtc *crtc = NULL;
        for (int i = 0; i < res->count_crtcs && crtc == NULL; i++) {
            drmModeCrtc *candidate = drmModeGetCrtc(fd, res->crtcs[i]);
            if (candidate != NULL && candidate->buffer_id != 0 && candidate->mode_valid) {
                crtc = candidate;
            } else if (candidate != NULL) {
                drmModeFreeCrtc(candidate);
            }
        }
        drmModeFreeResources(res);
        if (crtc == NULL) {
            fprintf(stderr, "no active vkms CRTC with a framebuffer\n");
            return 4;
        }

        drmModeFB2 *fb = drmModeGetFB2(fd, crtc->buffer_id);
        if (fb == NULL) {
            fprintf(stderr, "drmModeGetFB2 failed\n");
            return 1;
        }
        if (fb->pixel_format != DRM_FORMAT_XRGB8888) {
            fprintf(stderr, "scan-out format 0x%x is not XRGB8888\n", fb->pixel_format);
            return 5;
        }
        if (fb->handles[0] == 0 || fb->offsets[0] != 0) {
            fprintf(stderr, "scan-out framebuffer is not a linear dumb buffer\n");
            return 5;
        }

        struct drm_mode_map_dumb map;
        memset(&map, 0, sizeof map);
        map.handle = fb->handles[0];
        if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) != 0) {
            fprintf(stderr, "DRM_IOCTL_MODE_MAP_DUMB failed\n");
            return 1;
        }
        size_t size = (size_t)fb->pitches[0] * fb->height;
        uint8_t *base = mmap(NULL, size, PROT_READ, MAP_SHARED, fd, (off_t)map.offset);
        if (base == MAP_FAILED) {
            fprintf(stderr, "mmap of the scan-out buffer failed\n");
            return 1;
        }

        size_t row_bytes = (size_t)fb->width * 4;
        FILE *out = fopen(argv[1], "wb");
        if (out == NULL) {
            fprintf(stderr, "cannot open %s\n", argv[1]);
            return 1;
        }
        for (uint32_t y = 0; y < fb->height; y++) {
            if (fwrite(base + (size_t)y * fb->pitches[0], 1, row_bytes, out) != row_bytes) {
                fprintf(stderr, "short write to %s\n", argv[1]);
                return 1;
            }
        }
        if (fclose(out) != 0) {
            fprintf(stderr, "fclose failed\n");
            return 1;
        }

        printf("driver=vkms\n");
        printf("crtc_id=%u\n", crtc->crtc_id);
        printf("mode_w=%u\n", crtc->mode.hdisplay);
        printf("mode_h=%u\n", crtc->mode.vdisplay);
        printf("fb_id=%u\n", fb->fb_id);
        printf("fb_w=%u\n", fb->width);
        printf("fb_h=%u\n", fb->height);
        printf("fb_pitch=%u\n", fb->pitches[0]);

        munmap(base, size);
        drmModeFreeFB2(fb);
        drmModeFreeCrtc(crtc);
        close(fd);
        return 0;
    }
  '';

  vkmsScanoutCapture = pkgs.runCommandCC "vkms-scanout-capture" {
    buildInputs = [ pkgs.libdrm ];
  } ''
    mkdir -p $out/bin
    $CC -O2 -Wall -Wextra -Werror \
      -I${pkgs.libdrm.dev}/include/libdrm \
      -ldrm \
      ${vkmsScanoutCaptureSrc} -o $out/bin/vkms-scanout-capture
  '';
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-render-node";

  # The testScript imports the visual helper module via sys.path
  # injection at runtime, which the upstream nixos-test-driver type
  # checker can't trace statically.
  skipTypeCheck = true;

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        enable         = true;
        package        = halmasuit; # halmasuit-debug (frame_audit) via flake
        session.package   = halmasuit-session;
        greeterUid     = 999;
        greeterGroup   = "halmasuit-greeter";
        compositorUid  = 998;
        rendering.renderNode = "/dev/dri/renderD128";
        wallpaper = {
          type   = "image";
          source = ./fixtures/wallpaper.png;
        };
        # No greeter activity: this check reads the compositor's own
        # output, not greeter flow.
        greeterCommand = "${pkgs.writeShellScript "halmasuit-render-node-test-greeter" ''
          exec ${pkgs.coreutils}/bin/sleep infinity
        ''}";
      };

      # vgem provides the render-only node renderD128; vkms provides the
      # scan-out card with the connected Virtual-1 connector.
      boot.kernelModules = [ "vkms" "vgem" ];

      # Snapshot() runs inside halmasuit AFTER its privilege drop (uid
      # 998) and halmasuit runs under ProtectSystem=strict, so it needs
      # an explicitly-writable, non-PrivateTmp path to write the PNG.
      # Kept in sync with visual.py's GUEST_SNAPSHOT_DIR. The scan-out raw
      # capture is written to the same directory.
      systemd.tmpfiles.rules = [ "d /run/hsnap 0777 root root -" ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

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
        memorySize = 1024;
        cores      = 1;
        diskSize   = 1024;
        # No virtio-gpu or VGA device: Auto device selection must land on
        # vkms, the only card with a connected connector.
        qemu.options = [ "-vga none" ];
      };
    };

  testScript = ''
    import json
    import os
    import struct
    import sys
    import zlib

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")

    import visual

    def write_png_rgba(path, width, height, rgba):
        def chunk(tag, data):
            return (
                struct.pack(">I", len(data))
                + tag
                + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
            )

        stride = width * 4
        rows = b"".join(
            b"\x00" + bytes(rgba[y * stride:(y + 1) * stride])
            for y in range(height)
        )
        ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
        path.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(rows))
            + chunk(b"IEND", b"")
        )

    def xrgb_to_rgba(raw, pixels):
        # The scan-out buffer is little-endian XRGB8888: bytes B, G, R, X.
        rgba = bytearray(pixels * 4)
        rgba[0::4] = raw[2::4]
        rgba[1::4] = raw[1::4]
        rgba[2::4] = raw[0::4]
        rgba[3::4] = b"\xff" * pixels
        return rgba

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # scanout_active fires after the first frame is composited and queued.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'scanout_active'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "busctl --system status org.halmasuit",
        timeout=30,
    )

    # 1. The journal names the resolved render node, and the output is bound
    #    through the CPU-copy presenter rather than the single-device path.
    machine.succeed(
        "journalctl -b -u halmasuit -o cat --no-pager "
        "| grep -F 'render node resolved' "
        "| grep -qF '/dev/dri/renderD128'"
    )
    machine.succeed(
        "journalctl -b -u halmasuit -o cat --no-pager "
        "| grep -F 'DRM output: connector bound' "
        "| grep -Eq 'copy_scanout\\W*true'"
    )

    # 2. The vkms CRTC is active and scans out an XRGB8888 framebuffer at
    #    the CRTC's mode size. The helper exits non-zero until a flip has
    #    put a framebuffer on the CRTC, so this is a state-based wait.
    status = machine.wait_until_succeeds(
        "${vkmsScanoutCapture}/bin/vkms-scanout-capture /run/hsnap/vkms-scanout.raw",
        timeout=60,
    )
    scan = dict(
        line.split("=", 1) for line in status.splitlines() if "=" in line
    )
    print(json.dumps(scan, sort_keys=True))
    assert scan.get("driver") == "vkms", f"scan-out card is not vkms:\n{status}"
    mode = (int(scan["mode_w"]), int(scan["mode_h"]))
    fb = (int(scan["fb_w"]), int(scan["fb_h"]))
    assert fb == mode, (
        f"scan-out framebuffer {fb} does not match the CRTC mode {mode}:\n{status}"
    )
    w, h = fb
    pixels = w * h

    # 3. The pixels scanned out on vkms match halmasuit's Snapshot() of the
    #    same scene.
    machine.copy_from_machine("/run/hsnap/vkms-scanout.raw")
    raw = (machine.out_dir / "vkms-scanout.raw").read_bytes()
    assert len(raw) == pixels * 4, (
        f"raw scan-out is {len(raw)} bytes, expected {pixels * 4} for {w}x{h}"
    )
    scanout_png = machine.out_dir / "vkms-scanout.png"
    write_png_rgba(scanout_png, w, h, xrgb_to_rgba(raw, pixels))

    snapshot = visual.capture(machine, "halmasuit-render-node-snapshot")
    snap = snapshot.read_bytes()
    assert snap[12:16] == b"IHDR", "Snapshot() did not return a PNG"
    snap_size = struct.unpack(">II", snap[16:24])
    assert snap_size == fb, (
        f"Snapshot() is {snap_size}, scan-out is {fb}: the scenes differ in size"
    )

    score = visual.ssimulacra2_compare(snapshot, scanout_png)
    print(json.dumps({
        "check": "vkms scan-out vs Snapshot()",
        "score": score,
        "threshold": visual.EXACT_IMAGE_THRESHOLD,
    }))
    assert score >= visual.EXACT_IMAGE_THRESHOLD, (
        f"vkms scan-out score {score:.4f} < {visual.EXACT_IMAGE_THRESHOLD:.4f}\n"
        f"  snapshot: {snapshot}\n"
        f"  scan-out: {scanout_png}"
    )

    # 4. No flash across every frame_rendered, with the render node split.
    visual.assert_no_flash_stream(machine)

    print("halmasuit-render-node: ALL ASSERTIONS PASSED")
  '';
}
