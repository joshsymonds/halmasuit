# tests/halmasuit-recorder.nix — gate for
# `services.halmasuit.diagnostic.recordFrames`.
#
# Eval checks, forced at evaluation: a failing check throws, so the build
# fails before any VM starts. Contract:
# - recordFrames set, halmasuit-debug (frame_audit build): both units
#   export HALMASUIT_RECORD_FRAMES=<dir>; the rootfs unit lists <dir> in
#   ReadWritePaths and a tmpfiles rule creates <dir> 0700 owned by
#   compositorUid; the initramfs unit creates it in an ExecStartPre.
# - recordFrames set, halmasuit (no frame_audit build): evaluation fails
#   through a config.assertions entry whose message names recordFrames.
# - recordFrames null: each unit's environment and serviceConfig equal the
#   set unit's with exactly the recorder's entries removed
#   (HALMASUIT_RECORD_FRAMES, the initramfs directory-creating
#   ExecStartPre, rootfs ReadWritePaths), and nothing creates the recorder
#   directory.
#
# Implementation: synthetic nixosSystem evaluations of the module in its
# rootfs (enable) and boot-from-initrd (fromInitrd.enable) shapes.
#
# VM test (`halmasuit-recorder`), halmasuit-debug on three nodes:
# - `split`: render-node split, GL on vgem renderD128, scan-out on vkms,
#   no VGA device. Also asserts the last recorded PNG equals the vkms
#   scan-out byte for byte (RGB).
# - `direct`: single-device DrmCompositor path on virtio-gpu-pci. Also
#   asserts the last recorded PNG equals `Snapshot()` byte for byte (RGB).
# - `initrd`: `fromInitrd.enable` on virtio-gpu-pci, recording into
#   /run/halmasuit-lab/frames (a tmpfs that survives switch-root). Frames
#   recorded in stage 1 stay in the index after the pivot: one unbroken
#   seq run from 0, frames on both sides of rootfs_ready, every PNG at its
#   indexed size, dropped 0, the same halmasuit PID across both phases.
# Rootfs nodes: the diagnostic overlay is opened and closed (QEMU keyboard)
# so at least three frames are recorded, and the checks run once the
# recorder has caught up. Index starts at seq 0; the first frame_rendered
# is frame_id 0 and precedes scanout_active; seqs increase, each has one frame_rendered
# within 1 s of its mono_us, dropped is 0; halmasuit runs deprivileged; the
# recorder directories are owned by compositorUid; no flash stream.

{
  system,
  nixpkgs,
  halmasuit,
  halmasuit-debug,
  halmasuit-session,
  halmasuit-luks,
}:

let
  pkgs = import nixpkgs { inherit system; };
  lib = pkgs.lib;

  recordDir = "/var/lib/halmasuit/frames";
  # /run is moved across switch-root, so frames recorded in stage 1 stay
  # at this path after the pivot.
  labRecordDir = "/run/halmasuit-lab/frames";
  compositorUid = 998;

  evalWith = { shape, package, diagnostic }: nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ../nix/module.nix
      ({ ... }: {
        services.halmasuit = {
          package           = package;
          session.package   = halmasuit-session;
          luks.package      = halmasuit-luks;
          greeterUid        = 999;
          greeterGroup      = "halmasuit-greeter";
          compositorUid     = compositorUid;
          inherit diagnostic;
        } // (
          if shape == "rootfs"
          then { enable = true; }
          else { fromInitrd.enable = true; }
        );

        users.users.halmasuit-greeter = {
          isSystemUser = true;
          uid          = 999;
          group        = "halmasuit-greeter";
        };
        users.groups.halmasuit-greeter.gid = 999;
        users.users.halmasuit-compositor = {
          isSystemUser = true;
          uid          = compositorUid;
          group        = "halmasuit-greeter";
        };

        # Required-option stubs. Nothing reads these.
        fileSystems."/" = { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
        boot.loader.systemd-boot.enable = true;
        system.stateVersion = "26.05";
      })
    ];
  };

  rootfsWith = package: diagnostic: (evalWith { shape = "rootfs"; inherit package diagnostic; }).config;
  initrdWith = package: diagnostic: (evalWith { shape = "initrd"; inherit package diagnostic; }).config;

  rootUnit  = cfg: cfg.systemd.services.halmasuit;
  initrdUnit = cfg: cfg.boot.initrd.systemd.services.halmasuit;

  # ExecStartPre is a single string on the rootfs unit and a list on the
  # initramfs unit.
  preCmds = unit: lib.toList (unit.serviceConfig.ExecStartPre or [ ]);

  createsDir = cmd:
    lib.hasInfix "install -d -m 0700 -o ${toString compositorUid}" cmd
    && lib.hasSuffix " ${recordDir}" cmd;

  recordFramesFailure = a: !a.assertion && lib.hasInfix "recordFrames" a.message;

  rootSet      = rootfsWith halmasuit-debug { recordFrames = recordDir; };
  initrdSet    = initrdWith halmasuit-debug { recordFrames = recordDir; };
  rootSetPlain = rootfsWith halmasuit { recordFrames = recordDir; };

  rootNull     = rootfsWith halmasuit-debug { recordFrames = null; };
  initrdNull   = initrdWith halmasuit-debug { recordFrames = null; };

  # The set unit with the recorder's own entries removed. Null must equal
  # it exactly, so the option touches nothing else in either unit.
  withoutRecorderEnv = unit: builtins.removeAttrs unit.environment [ "HALMASUIT_RECORD_FRAMES" ];
  rootSetBareConfig = builtins.removeAttrs (rootUnit rootSet).serviceConfig [ "ReadWritePaths" ];

  tmpfilesDir = cfg: (cfg.systemd.tmpfiles.settings.halmasuit-record-frames or { }).${recordDir}.d or null;
  initrdSetBareConfig = (initrdUnit initrdSet).serviceConfig // {
    ExecStartPre = lib.filter (c: !(createsDir c)) (preCmds (initrdUnit initrdSet));
  };

  checks = [
    {
      name = "set: rootfs environment exports HALMASUIT_RECORD_FRAMES";
      ok = ((rootUnit rootSet).environment.HALMASUIT_RECORD_FRAMES or null) == recordDir;
    }
    {
      name = "set: initramfs environment exports HALMASUIT_RECORD_FRAMES";
      ok = ((initrdUnit initrdSet).environment.HALMASUIT_RECORD_FRAMES or null) == recordDir;
    }
    {
      name = "set: rootfs ReadWritePaths contains the recorder directory";
      ok = lib.elem recordDir ((rootUnit rootSet).serviceConfig.ReadWritePaths or [ ]);
    }
    {
      name = "set: a rootfs tmpfiles rule creates the directory 0700 owned by compositorUid";
      ok = let d = tmpfilesDir rootSet; in
        d != null && d.mode == "0700" && d.user == toString compositorUid;
    }
    {
      name = "set: initramfs ExecStartPre creates the directory 0700 owned by compositorUid";
      ok = lib.any createsDir (preCmds (initrdUnit initrdSet));
    }
    {
      name = "set without frame_audit build: evaluation fails naming recordFrames";
      ok = lib.any recordFramesFailure rootSetPlain.assertions;
    }
    {
      name = "set with frame_audit build: no recordFrames assertion fails";
      ok = !(lib.any recordFramesFailure rootSet.assertions);
    }
    {
      name = "null: rootfs environment equals the set one without HALMASUIT_RECORD_FRAMES";
      ok = (rootUnit rootNull).environment == withoutRecorderEnv (rootUnit rootSet);
    }
    {
      name = "null: rootfs serviceConfig equals the set one without ReadWritePaths";
      ok = (rootUnit rootNull).serviceConfig == rootSetBareConfig;
    }
    {
      name = "null: initramfs environment equals the set one without HALMASUIT_RECORD_FRAMES";
      ok = (initrdUnit initrdNull).environment == withoutRecorderEnv (initrdUnit initrdSet);
    }
    {
      name = "null: initramfs serviceConfig equals the set one without the directory-creating ExecStartPre";
      ok = (initrdUnit initrdNull).serviceConfig == initrdSetBareConfig;
    }
    {
      name = "null: neither unit exports HALMASUIT_RECORD_FRAMES or creates a recorder directory";
      ok = !((rootUnit rootNull).environment ? HALMASUIT_RECORD_FRAMES)
        && !((initrdUnit initrdNull).environment ? HALMASUIT_RECORD_FRAMES)
        && tmpfilesDir rootNull == null
        && !(lib.any createsDir (preCmds (initrdUnit initrdNull)));
    }
  ];

  failures = map (c: c.name) (lib.filter (c: !c.ok) checks);

  report =
    if failures == [ ]
    then "halmasuit-recorder: PASS (${toString (builtins.length checks)} checks)"
    else throw "halmasuit-recorder: FAIL: ${lib.concatStringsSep "; " failures}";
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

  # Guest-side RGB comparison. Each spec is `png:<path>` (any PNG, alpha
  # dropped) or `xrgb:<path>:<W>x<H>` (packed XRGB8888 rows, the vkms
  # scan-out capture). Exits non-zero unless both decode to identical RGB.
  rgbCompare = pkgs.writers.writePython3Bin "halmasuit-rgb-compare" {
    libraries = [ pkgs.python3Packages.pillow ];
  } ''
    import sys

    from PIL import Image


    def load(spec):
        kind, _, rest = spec.partition(":")
        if kind == "png":
            return Image.open(rest).convert("RGB")
        if kind == "xrgb":
            path, _, dims = rest.rpartition(":")
            width, height = (int(v) for v in dims.split("x"))
            with open(path, "rb") as f:
                raw = f.read()
            if len(raw) != width * height * 4:
                sys.exit(
                    f"{path}: {len(raw)} bytes, expected "
                    f"{width}x{height} XRGB8888"
                )
            rgb = bytearray(width * height * 3)
            rgb[0::3] = raw[2::4]
            rgb[1::3] = raw[1::4]
            rgb[2::3] = raw[0::4]
            return Image.frombytes("RGB", (width, height), bytes(rgb))
        sys.exit(f"unknown spec kind {kind!r} in {spec!r}")


    def main():
        if len(sys.argv) != 3:
            sys.exit("usage: halmasuit-rgb-compare <spec> <spec>")
        left = load(sys.argv[1])
        right = load(sys.argv[2])
        if left.size != right.size:
            sys.exit(f"size mismatch: {left.size} vs {right.size}")
        a = left.tobytes()
        b = right.tobytes()
        if a != b:
            width = left.size[0]
            index = next(i for i, (x, y) in enumerate(zip(a, b)) if x != y)
            pixel = index // 3
            sys.exit(
                f"RGB bytes differ at byte {index} "
                f"(pixel x={pixel % width}, y={pixel // width}, "
                f"channel {index % 3})"
            )
        print(f"match: {left.size[0]}x{left.size[1]}, "
              f"{len(a)} RGB bytes identical")


    main()
  '';

  # Exits 0 once the recorder has caught up with the render loop: at least
  # three frames are indexed and the last index seq is the newest
  # frame_rendered frame_id. Taken after the scene stops changing, so the
  # last recorded frame is the one on screen. Argument: the record dir.
  recorderSettled = pkgs.writeShellScript "halmasuit-recorder-settled" ''
    set -eu
    index=$1/index.jsonl
    [ "$(${pkgs.coreutils}/bin/wc -l < "$index")" -ge 3 ]
    last=$(${pkgs.coreutils}/bin/tail -n 1 "$index" | ${pkgs.jq}/bin/jq -r .seq)
    newest=$(journalctl -b -o cat --no-pager \
      | ${pkgs.jq}/bin/jq -Rr 'fromjson? | .fields.json // empty | fromjson? | select(.event == "frame_rendered") | .frame_id' \
      | ${pkgs.coreutils}/bin/sort -n | ${pkgs.coreutils}/bin/tail -n 1)
    [ -n "$newest" ] && [ "$last" = "$newest" ]
  '';

  # No greeter activity: the checks read the compositor's own output.
  greeterScript = pkgs.writeShellScript "halmasuit-recorder-test-greeter" ''
    exec ${pkgs.coreutils}/bin/sleep infinity
  '';

  # Shared by all nodes. Snapshot() runs after halmasuit's privilege drop
  # under ProtectSystem=strict, so /run/hsnap is the writable scratch dir.
  recorderBase = {
    imports = [ ../nix/module.nix ];

    services.halmasuit = {
      package           = halmasuit-debug; # frame_audit build
      session.package   = halmasuit-session;
      luks.package      = halmasuit-luks;
      greeterUid        = 999;
      greeterGroup      = "halmasuit-greeter";
      compositorUid     = compositorUid;
      wallpaper = {
        type   = "image";
        source = ./fixtures/wallpaper.png;
      };
      greeterCommand = "${greeterScript}";
    };

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
      uid          = compositorUid;
      group        = "halmasuit-greeter";
    };

    environment.systemPackages = [ rgbCompare ];

    virtualisation = {
      memorySize = 1024;
      cores      = 1;
      diskSize   = 1024;
    };
  };

  vmTest = pkgs.testers.runNixOSTest {
    name = "halmasuit-recorder";

    # The testScript imports the visual helper via sys.path injection.
    skipTypeCheck = true;

    nodes = {
      # Render-node split: vgem provides renderD128 (render-only, GL),
      # vkms the scan-out card. No VGA or virtio-gpu device exists.
      split = {
        imports = [ recorderBase ];
        services.halmasuit = {
          enable = true;
          diagnostic.recordFrames = recordDir;
          rendering.renderNode = "/dev/dri/renderD128";
        };
        boot.kernelModules = [ "vkms" "vgem" ];
        # The virtio keyboard drives the diagnostic-overlay chord.
        virtualisation.qemu.options = [ "-vga none" "-device virtio-keyboard-pci" ];
      };
      # Single-device DrmCompositor path.
      direct = {
        imports = [ recorderBase ];
        services.halmasuit = {
          enable = true;
          diagnostic.recordFrames = recordDir;
        };
        virtualisation.qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          "-device virtio-keyboard-pci"
        ];
      };
      # Boot-from-initrd: halmasuit starts in stage 1 and records into a
      # /run directory that survives switch-root.
      initrd = {
        imports = [ recorderBase ];
        services.halmasuit = {
          fromInitrd.enable = true;
          diagnostic.recordFrames = labRecordDir;
        };
        # The greeter is spawned post-pivot from the rootfs closure, so
        # the script must be pinned there (as tests/initrd-survival.nix does).
        system.extraDependencies = [ greeterScript ];
        virtualisation.qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
          "-device virtio-keyboard-pci"
        ];
      };
    };

    testScript = ''
      import base64
      import json
      import struct
      import sys

      sys.path.insert(0, "${./lib}")
      import visual

      RECORD_DIR = "${recordDir}"
      LAB_RECORD_DIR = "${labRecordDir}"
      COMPOSITOR_UID = ${toString compositorUid}
      SNAP_DIR = "/run/hsnap"
      MAX_SKEW_US = 1_000_000
      RGB_COMPARE = "${rgbCompare}/bin/halmasuit-rgb-compare"
      VKMS_CAPTURE = "${vkmsScanoutCapture}/bin/vkms-scanout-capture"
      RECORDER_SETTLED = "${recorderSettled}"


      def journal_events(node):
          """(CLOCK_MONOTONIC usec, _PID, event dict) for every halmasuit
          event, from the whole boot: the initramfs unit's journal entries
          are not reachable by unit name after switch-root."""
          raw = node.succeed("journalctl -b -o json --no-pager")
          events = []
          for line in raw.splitlines():
              entry = json.loads(line)
              msg = entry.get("MESSAGE")
              if not isinstance(msg, str) or not msg.startswith("{"):
                  continue
              try:
                  outer = json.loads(msg)
              except ValueError:
                  continue
              inner = outer.get("fields", {}).get("json")
              if not inner:
                  continue
              try:
                  ev = json.loads(inner)
              except ValueError:
                  continue
              if isinstance(ev, dict) and "event" in ev:
                  events.append((
                      int(entry["__MONOTONIC_TIMESTAMP"]),
                      int(entry["_PID"]),
                      ev,
                  ))
          return events


      def png_size(node, path):
          """(width, height) from the IHDR chunk of the PNG at `path`."""
          head = base64.b64decode(node.succeed(f"head -c 24 {path} | base64 -w0"))
          assert head[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG"
          return struct.unpack(">II", head[16:24])


      def change_scene(node, record_dir):
          """Open and close the diagnostic overlay: two damaged frames after
          frame 0, then a static scene the recorder must catch up with."""
          node.send_key("ctrl-alt-shift-esc")
          node.wait_until_succeeds(
              "journalctl -b -o cat | grep -qF "
              "'OVERLAY_OPEN: diagnostic content composed'",
              timeout=30,
          )
          node.send_key("esc")
          node.wait_until_succeeds(
              "journalctl -b -o cat | grep -qF "
              "'OVERLAY_CLOSE: diagnostic overlay hidden'",
              timeout=30,
          )
          node.wait_until_succeeds(f"{RECORDER_SETTLED} {record_dir}", timeout=60)


      def check_initrd_recording(node):
          """Frames recorded in stage 1 survive switch-root: one unbroken
          index from seq 0, frames on both sides of rootfs_ready, every
          PNG at its indexed size, all logged by the same halmasuit PID."""
          change_scene(node, LAB_RECORD_DIR)
          index = [
              json.loads(line)
              for line in node.succeed(
                  f"cat {LAB_RECORD_DIR}/index.jsonl"
              ).splitlines()
          ]
          assert len(index) >= 3, (
              f"index.jsonl has {len(index)} lines, expected at least 3"
          )

          events = journal_events(node)
          started = [ev["pid"] for _, _, ev in events if ev.get("event") == "started"]
          assert len(started) == 1, f"started events: {started}"
          phases = {}
          for ts, pid, ev in events:
              if ev.get("event") == "phase_entered" and ev.get("phase") in (
                  "initramfs_init",
                  "rootfs_ready",
              ):
                  phases.setdefault(ev["phase"], []).append((ts, pid))
          for phase in ("initramfs_init", "rootfs_ready"):
              assert len(phases.get(phase, [])) == 1, (
                  f"phase {phase} entries: {phases.get(phase)}"
              )
          init_ts, init_pid = phases["initramfs_init"][0]
          ready_ts, ready_pid = phases["rootfs_ready"][0]
          assert init_ts < ready_ts, (
              f"initramfs_init at {init_ts} us is not before rootfs_ready at "
              f"{ready_ts} us"
          )
          pid = started[0]
          assert init_pid == pid and ready_pid == pid, (
              f"phases logged by pids {init_pid} and {ready_pid}, "
              f"started pid {pid}"
          )
          node.succeed(f"kill -0 {pid}")

          seqs = [line["seq"] for line in index]
          assert seqs == list(range(len(seqs))), (
              f"index seqs are not contiguous from 0: {seqs}"
          )
          assert all(line["dropped"] == 0 for line in index), (
              f"frames dropped: {[l for l in index if l['dropped']]}"
          )
          before = [line for line in index if line["mono_us"] < ready_ts]
          after = [line for line in index if line["mono_us"] > ready_ts]
          assert before, f"no frame recorded before rootfs_ready at {ready_ts} us"
          assert after, f"no frame recorded after rootfs_ready at {ready_ts} us"

          for line in index:
              path = f"{LAB_RECORD_DIR}/{line['output']}/{line['seq']:08d}.png"
              size = png_size(node, path)
              assert size == (line["width"], line["height"]), (
                  f"{path} is {size}, index says "
                  f"{line['width']}x{line['height']}"
              )


      def check_recording(node, is_split):
          change_scene(node, RECORD_DIR)
          index = [
              json.loads(line)
              for line in node.succeed(
                  f"cat {RECORD_DIR}/index.jsonl"
              ).splitlines()
          ]
          assert len(index) >= 3, (
              f"index.jsonl has {len(index)} lines, expected frame 0 plus "
              "the overlay open and close frames"
          )

          # halmasuit runs deprivileged as the compositor uid.
          pid = node.succeed(
              "systemctl show -p MainPID --value halmasuit.service"
          ).strip()
          assert pid.isdigit() and int(pid) > 0, f"MainPID {pid!r}"
          uids = node.succeed(
              f"awk '/^Uid:/ {{print $2,$3,$4,$5}}' /proc/{pid}/status"
          ).split()
          assert uids == [str(COMPOSITOR_UID)] * 4, (
              f"halmasuit Uid line {uids}, expected compositorUid on all four"
          )

          # The recorder directories are owned by compositorUid.
          outputs = sorted({line["output"] for line in index})
          for path in [RECORD_DIR] + [f"{RECORD_DIR}/{o}" for o in outputs]:
              owner = node.succeed(f"stat -c %u {path}").strip()
              assert owner == str(COMPOSITOR_UID), (
                  f"{path} owned by uid {owner}, expected {COMPOSITOR_UID}"
              )

          seqs = [line["seq"] for line in index]
          assert seqs[0] == 0, f"first index seq is {seqs[0]}, expected 0"
          assert all(a < b for a, b in zip(seqs, seqs[1:])), (
              f"index seqs do not increase: {seqs}"
          )
          assert all(line["dropped"] == 0 for line in index), (
              f"frames dropped: {[l for l in index if l['dropped']]}"
          )
          assert all(line["format"] == "rgb8" for line in index), (
              "index format is not rgb8"
          )

          # Timeline: frame_id == seq, first frame_rendered is frame 0,
          # first recorded frame precedes scanout_active.
          events = journal_events(node)
          scanout = [
              ts for ts, _, ev in events
              if ev.get("event") == "phase_entered"
              and ev.get("phase") == "scanout_active"
          ]
          assert scanout, "no scanout_active phase_entered event"
          rendered = [
              (ts, ev["frame_id"]) for ts, _, ev in events
              if ev.get("event") == "frame_rendered"
          ]
          assert rendered, "no frame_rendered events"
          assert rendered[0][1] == 0, (
              f"first frame_rendered is frame_id {rendered[0][1]}, expected 0"
          )
          assert index[0]["mono_us"] < scanout[0], (
              f"first recorded frame at {index[0]['mono_us']} us does not "
              f"precede scanout_active at {scanout[0]} us"
          )
          by_id = {}
          for ts, frame_id in rendered:
              by_id.setdefault(frame_id, []).append(ts)
          for line in index:
              stamps = by_id.get(line["seq"], [])
              assert len(stamps) == 1, (
                  f"seq {line['seq']} has {len(stamps)} frame_rendered events"
              )
              skew = abs(stamps[0] - line["mono_us"])
              assert skew <= MAX_SKEW_US, (
                  f"seq {line['seq']}: frame_rendered {stamps[0]} us vs "
                  f"mono_us {line['mono_us']} us, skew {skew} us"
              )

          last = index[-1]
          last_png = f"{RECORD_DIR}/{last['output']}/{last['seq']:08d}.png"

          if is_split:
              # The CPU-copy presenter is bound, and the last recorded
              # frame is byte-identical to the vkms scan-out.
              node.succeed(
                  "journalctl -b -u halmasuit -o cat --no-pager "
                  "| grep -F 'DRM output: connector bound' "
                  "| grep -Eq 'copy_scanout\\W*true'"
              )
              status = node.wait_until_succeeds(
                  f"{VKMS_CAPTURE} {SNAP_DIR}/recorder-vkms-scanout.raw",
                  timeout=60,
              )
              scan = dict(
                  line.split("=", 1)
                  for line in status.splitlines()
                  if "=" in line
              )
              assert scan.get("driver") == "vkms", f"scan-out: {status}"
              fb = (int(scan["fb_w"]), int(scan["fb_h"]))
              mode = (int(scan["mode_w"]), int(scan["mode_h"]))
              assert fb == mode, f"scan-out fb {fb} != CRTC mode {mode}"
              node.succeed(
                  f"{RGB_COMPARE} png:{last_png} "
                  f"xrgb:{SNAP_DIR}/recorder-vkms-scanout.raw:{fb[0]}x{fb[1]}"
              )
          else:
              # The last recorded frame is byte-identical to Snapshot().
              snap = f"{SNAP_DIR}/recorder-snapshot.png"
              node.succeed(
                  "busctl --system call org.halmasuit "
                  "/org/halmasuit/Debug/Introspect "
                  f"org.halmasuit.Debug.Introspect Snapshot s {snap}"
              )
              node.succeed(f"{RGB_COMPARE} png:{last_png} png:{snap}")

          visual.assert_no_flash_stream(node)


      start_all()
      for node in [split, direct]:
          node.wait_for_unit("multi-user.target")
          node.wait_for_unit("halmasuit.service")
          # scanout_active fires after the first frame is composited.
          node.wait_until_succeeds(
              "journalctl -u halmasuit | grep -qF 'scanout_active'",
              timeout=30,
          )
          node.wait_until_succeeds(
              "busctl --system status org.halmasuit", timeout=30
          )

      initrd.wait_for_unit("multi-user.target")
      initrd.wait_until_succeeds(
          "journalctl -b -o cat | grep -qF 'scanout_active'",
          timeout=30,
      )
      initrd.wait_until_succeeds(
          "busctl --system status org.halmasuit", timeout=30
      )

      check_recording(split, is_split=True)
      check_recording(direct, is_split=False)
      check_initrd_recording(initrd)

      print("halmasuit-recorder: ALL ASSERTIONS PASSED")
    '';
  };
in
# A failed eval check throws here, so the VM test is only built when the
# eval checks pass.
builtins.seq report vmTest
