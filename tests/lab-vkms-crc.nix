# lab-vkms-crc gate (I2): halmasuit.lab.vkmsCrc runs the vkms CRC sampler in
# stage 1 and after switch-root. vkms is the only display and the fbdev console
# is kept off it. A dumb-buffer mode set shows a black frame whose CRC matches
# the reference computed from vkms's composition rule, and a lit frame whose
# CRC differs. Real vkms and real DRM, no mocks.
{
  system,
  nixpkgs,
  labModule,
}:

let
  pkgs = import nixpkgs {
    inherit system;
  };

  # Scanout helper: a dumb-buffer mode set on card0 that holds the mode until it
  # is killed. It shows a black frame, then fills the buffer white on SIGUSR1.
  # Each step prints its monotonic time before the DRM call it names, so the
  # test can order the sampler's samples against those calls.
  vkmsScanout = pkgs.runCommandCC "vkms-scanout" {
    buildInputs = [ pkgs.libdrm ];
    nativeBuildInputs = [ pkgs.pkg-config ];
    src = pkgs.writeText "vkms-scanout.c" ''
      #include <drm.h>
      #include <xf86drm.h>
      #include <xf86drmMode.h>

      #include <errno.h>
      #include <fcntl.h>
      #include <inttypes.h>
      #include <signal.h>
      #include <stdint.h>
      #include <stdio.h>
      #include <stdlib.h>
      #include <string.h>
      #include <sys/mman.h>
      #include <time.h>
      #include <unistd.h>

      static _Noreturn void fatal(const char *what, int err)
      {
      	if (err != 0)
      		fprintf(stderr, "vkms-scanout: %s: %s\n", what, strerror(err));
      	else
      		fprintf(stderr, "vkms-scanout: %s\n", what);
      	exit(EXIT_FAILURE);
      }

      static uint64_t monotonic_us(void)
      {
      	struct timespec ts;

      	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
      		fatal("clock_gettime", errno);
      	return (uint64_t)ts.tv_sec * 1000000u + (uint64_t)ts.tv_nsec / 1000u;
      }

      int main(void)
      {
      	sigset_t usr1;
      	uint32_t handle, pitch, fb_id, crtc_id, conn_id;
      	uint64_t size, offset;
      	uint32_t *pixels;
      	drmModeRes *res;
      	drmModeConnector *conn;
      	drmModeModeInfo mode;
      	int fd, sig, rc;

      	setvbuf(stdout, NULL, _IOLBF, 0);

      	/* Blocked before any mode set so a SIGUSR1 sent later stays pending. */
      	sigemptyset(&usr1);
      	sigaddset(&usr1, SIGUSR1);
      	if (sigprocmask(SIG_BLOCK, &usr1, NULL) != 0)
      		fatal("sigprocmask", errno);

      	fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
      	if (fd < 0)
      		fatal("open /dev/dri/card0", errno);
      	if (drmSetMaster(fd) != 0)
      		fatal("drmSetMaster", errno);

      	res = drmModeGetResources(fd);
      	if (res == NULL)
      		fatal("drmModeGetResources", errno);
      	if (res->count_connectors < 1 || res->count_crtcs < 1)
      		fatal("no connector or CRTC", 0);
      	conn_id = res->connectors[0];
      	crtc_id = res->crtcs[0];

      	conn = drmModeGetConnector(fd, conn_id);
      	if (conn == NULL)
      		fatal("drmModeGetConnector", errno);
      	if (conn->count_modes < 1)
      		fatal("connector reports no modes", 0);
      	mode = conn->modes[0];

      	if (drmModeCreateDumbBuffer(fd, mode.hdisplay, mode.vdisplay, 32, 0,
      				    &handle, &pitch, &size) != 0)
      		fatal("drmModeCreateDumbBuffer", errno);
      	if (drmModeAddFB(fd, mode.hdisplay, mode.vdisplay, 24, 32, pitch,
      			 handle, &fb_id) != 0)
      		fatal("drmModeAddFB", errno);
      	if (drmModeMapDumbBuffer(fd, handle, &offset) != 0)
      		fatal("drmModeMapDumbBuffer", errno);
      	pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
      		      (off_t)offset);
      	if (pixels == MAP_FAILED)
      		fatal("mmap", errno);
      	memset(pixels, 0, size);

      	printf("mode-set t=%" PRIu64 "\n", monotonic_us());
      	if (drmModeSetCrtc(fd, crtc_id, fb_id, 0, 0, &conn_id, 1, &mode) != 0)
      		fatal("drmModeSetCrtc", errno);

      	rc = sigwait(&usr1, &sig);
      	if (rc != 0)
      		fatal("sigwait", rc);
      	printf("lit t=%" PRIu64 "\n", monotonic_us());
      	for (uint64_t i = 0; i < size / sizeof *pixels; i++)
      		pixels[i] = 0x00ffffff;

      	for (;;)
      		pause();
      }
    '';
  } ''
    mkdir -p $out/bin
    $CC -Wall -Wextra -Werror -O2 $src $(pkg-config --cflags --libs libdrm) \
      -o $out/bin/vkms-scanout
  '';
in
pkgs.testers.runNixOSTest {
  name = "lab-vkms-crc";

  nodes.machine =
    { ... }:
    {
      imports = [
        labModule
        ./lib/test-user.nix
      ];

      virtualisation = {
        memorySize = 1024;
        cores = 1;
        # vkms is the only display.
        qemu.options = [ "-vga none" ];
      };

      # systemd-in-initramfs is required for boot.initrd.systemd.services.
      boot.initrd.systemd.enable = true;

      # vkms loads in stage 1 so the initrd unit can find it; it stays loaded
      # across switch-root.
      boot.initrd.availableKernelModules = [ "vkms" ];
      boot.initrd.kernelModules = [ "vkms" ];

      # An empty drm_client_lib.active registers no in-kernel DRM client, so
      # fbcon never attaches the console to vkms.
      boot.kernelParams = [ "drm_client_lib.active=" ];

      halmasuit.lab.vkmsCrc.enable = true;
    };

  testScript = ''
    import re
    import zlib

    # vkms's preferred mode on its one CRTC.
    WIDTH, HEIGHT = 1024, 768

    # vkms composes each row as struct pixel_argb_u16 {u16 a, r, g, b}, little
    # endian, background a=0xffff r=g=b=0, and CRCs it with crc32_le seed 0 and
    # no final inversion. zlib.crc32 inverts its start value and its result, so
    # crc32_le(0, data) is ~zlib.crc32(data, 0xffffffff).
    BLACK_ROW = bytes([0xFF, 0xFF, 0, 0, 0, 0, 0, 0]) * WIDTH
    BLACK = (~zlib.crc32(BLACK_ROW * HEIGHT, 0xFFFFFFFF)) & 0xFFFFFFFF

    SAMPLE = re.compile(
        r"^sample card=(\S+) crtc=(\d+) t=(\d+) crc=([0-9a-f]{8}) active=([01])$",
        re.M,
    )

    def samples(text):
        return [
            {
                "t": int(m[3]),
                "crc": int(m[4], 16),
                "active": m[5] == "1",
            }
            for m in SAMPLE.finditer(text)
        ]

    def field(pattern, text):
        m = re.search(pattern, text, re.M)
        if m is None:
            raise AssertionError(f"no line matches {pattern!r} in:\n{text}")
        return int(m[1])

    def unit_journal(unit):
        return machine.succeed(
            f"journalctl -b -o cat _SYSTEMD_UNIT={unit} SYSLOG_IDENTIFIER=lab-vkms-crc"
        )

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Both units start, and the stage-1 unit samples the CRTC while it is off.
    machine.wait_until_succeeds(
        "journalctl -b -o cat _SYSTEMD_UNIT=lab-vkms-crc-initrd.service | grep -q '^start t='",
        timeout=300,
    )
    machine.wait_until_succeeds(
        "journalctl -b -o cat _SYSTEMD_UNIT=lab-vkms-crc.service | grep -q '^sample card=card0 crtc=.* active=0$'",
        timeout=300,
    )
    machine.wait_until_succeeds(
        "journalctl -b -o cat _SYSTEMD_UNIT=lab-vkms-crc-initrd.service | grep -q '^sample card=card0 crtc=.* active=0$'",
        timeout=300,
    )

    # Mode set with a black frame, then a lit frame on SIGUSR1.
    machine.succeed("systemd-run --unit=vkms-scanout ${vkmsScanout}/bin/vkms-scanout")
    machine.wait_until_succeeds(
        "journalctl -b -o cat _SYSTEMD_UNIT=vkms-scanout.service | grep -q '^mode-set t='",
        timeout=300,
    )
    machine.wait_until_succeeds(
        f"journalctl -b -o cat SYSLOG_IDENTIFIER=lab-vkms-crc | grep -q 'crc={BLACK:08x} active=1$'",
        timeout=300,
    )

    machine.succeed("systemctl kill --signal=SIGUSR1 vkms-scanout.service")
    machine.wait_until_succeeds(
        "journalctl -b -o cat _SYSTEMD_UNIT=vkms-scanout.service | grep -q '^lit t='",
        timeout=300,
    )
    machine.wait_until_succeeds(
        "journalctl -b -o cat SYSLOG_IDENTIFIER=lab-vkms-crc"
        f" | grep -E '^sample .* active=1$' | grep -v 'crc={BLACK:08x}' | grep -q .",
        timeout=300,
    )

    scanout = machine.succeed("journalctl -b -o cat _SYSTEMD_UNIT=vkms-scanout.service")
    mode_set_t = field(r"^mode-set t=(\d+)$", scanout)
    lit_t = field(r"^lit t=(\d+)$", scanout)

    initrd = samples(unit_journal("lab-vkms-crc-initrd.service"))
    rootfs = samples(unit_journal("lab-vkms-crc.service"))
    everything = initrd + rootfs

    # The stage-1 unit sampled the CRTC, and every sample before the mode set
    # reports it inactive with crc 00000000.
    if not initrd:
        raise AssertionError("stage-1 unit logged no sample")
    before = [s for s in everything if s["t"] < mode_set_t]
    if not before or any(s["active"] or s["crc"] != 0 for s in before):
        raise AssertionError(f"samples before the mode set are not all inactive: {before}")

    # Every active sample before the lit fill is the black frame, with the
    # computed reference CRC.
    black = [s for s in everything if s["active"] and s["t"] < lit_t]
    if not black:
        raise AssertionError("no active sample of the black frame")
    wrong = [s for s in black if s["crc"] != BLACK]
    if wrong:
        raise AssertionError(
            f"black frame CRC is not {BLACK:08x}: {[hex(s['crc']) for s in wrong]}"
        )

    # The lit frame reports a different CRC.
    lit = [s for s in everything if s["active"] and s["t"] >= lit_t and s["crc"] != BLACK]
    if not lit:
        raise AssertionError("no active sample of the lit frame differing from the black CRC")

    failures = machine.execute(
        "journalctl -b -o cat SYSLOG_IDENTIFIER=lab-vkms-crc | grep '^lab-vkms-crc:'"
    )[1]
    if failures.strip():
        raise AssertionError(f"lab-vkms-crc reported failures:\n{failures}")

    # The console never attached to vkms.
    machine.succeed("test ! -e /sys/class/graphics/fb0")

    # The rootfs unit keeps running after switch-root.
    machine.succeed("systemctl is-active lab-vkms-crc.service")
  '';
}
