# Reads the NVIDIA hardware per-head scanout CRC via the private
# DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2 ioctl (Epic #45 rung 4). This is a
# TRUE post-NVIDIA signal: the display engine computes the CRC of the
# actual scanned-out frame in hardware; we just read it. The ioctl is
# DRM_RENDER_ALLOW (no master/auth needed), so this standalone process
# reads the CRC for the head halmasuit is driving without being DRM
# master.
#
# Args: <crtc_id> <seconds> <card-path>
# Prints key=value lines + the distinct compositor CRCs seen.
import sys, os, fcntl, struct, time

crtc_id = int(sys.argv[1])
secs = float(sys.argv[2])
card = sys.argv[3]

# _IOWR('d', DRM_COMMAND_BASE + NVIDIA_GET_CRTC_CRC32_V2(0x0c), 28)
DIR_RW = 3
TYPE = ord("d")
NR = 0x40 + 0x0C
SIZE = 28
req = (DIR_RW << 30) | (SIZE << 16) | (TYPE << 8) | NR
if req >= 2 ** 31:  # fcntl wants a signed-fitting int for the unsigned op
    req -= 2 ** 32

# struct drm_nvidia_get_crtc_crc32_v2_params:
#   u32 crtc_id; then 3x { u32 value; u8 supported; u8 pad0; u16 pad1 }
FMT = "<IIBBHIBBHIBBH"

fd = os.open(card, os.O_RDWR)
# Track all three taps independently: compositor / rasterGenerator / output.
distinct = {"comp": set(), "rg": set(), "out": set()}
sup = {"comp": None, "rg": None, "out": None}
samples = 0
err = None
deadline = time.monotonic() + secs
while time.monotonic() < deadline:
    buf = bytearray(struct.pack(FMT, crtc_id, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
    try:
        fcntl.ioctl(fd, req, buf, True)
    except OSError as e:
        err = e.errno
        break
    v = struct.unpack(FMT, bytes(buf))
    sup["comp"], sup["rg"], sup["out"] = v[2], v[6], v[10]
    distinct["comp"].add(v[1])
    distinct["rg"].add(v[5])
    distinct["out"].add(v[9])
    samples += 1
    time.sleep(0.002)
os.close(fd)

if err is not None:
    print("ioctl_err=" + str(err))
    sys.exit(0)
print("samples=" + str(samples))
for tap in ("comp", "rg", "out"):
    vals = sorted(distinct[tap])
    print(
        f"{tap}_supported={sup[tap]} {tap}_distinct={len(vals)} "
        f"{tap}_values=" + ",".join(hex(x) for x in vals[:6])
    )
