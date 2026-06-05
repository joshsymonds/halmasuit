# Probes whether the GPU is actually scanning out (generating vblanks)
# via the standard DRM_IOCTL_WAIT_VBLANK with a RELATIVE-0 query, which
# returns the current vblank sequence without blocking. Sampling it ~1s
# apart gives the real scanout refresh rate per pipe (Epic #45 rung 4).
#
# If the rate is ~60/s, the timing generator is running (so a 0x0 CRC is
# a patch/wait-logic issue); if it's ~0, the GPU isn't truly scanning
# out (no physical sink) and the per-vblank HW CRC will never populate.
#
# Args: <card-path>
import sys, os, fcntl, struct, time

card = sys.argv[1]

# DRM_IOCTL_WAIT_VBLANK = DRM_IOWR('d', 0x3a, union drm_wait_vblank).
# union size on LP64 = max(request 16, reply 24) = 24.
TYPE = ord("d")
NR = 0x3A
SIZE = 24
req = (3 << 30) | (SIZE << 16) | (TYPE << 8) | NR
if req >= 2 ** 31:
    req -= 2 ** 32

_DRM_VBLANK_RELATIVE = 0
_DRM_VBLANK_HIGH_CRTC_SHIFT = 1

fd = os.open(card, os.O_RDWR)

def seq(pipe):
    typ = _DRM_VBLANK_RELATIVE | (pipe << _DRM_VBLANK_HIGH_CRTC_SHIFT)
    # request: u32 type, u32 sequence, u64 signal — padded to the 24B union.
    buf = bytearray(struct.pack("<IIQ", typ, 0, 0))
    buf += b"\x00" * (SIZE - len(buf))
    try:
        fcntl.ioctl(fd, req, buf, True)
    except OSError as e:
        return ("err", e.errno)
    # reply: i32 type, u32 sequence, i64 tval_sec, i64 tval_usec.
    r = struct.unpack("<IIqq", bytes(buf[:SIZE]))
    return ("ok", r[1])

for pipe in (0, 1):
    a = seq(pipe)
    time.sleep(1.0)
    b = seq(pipe)
    if a[0] == "ok" and b[0] == "ok":
        print(f"pipe{pipe}_vblank_rate={b[1] - a[1]} (seq {a[1]}->{b[1]})")
    else:
        bad = a[1] if a[0] == "err" else b[1]
        print(f"pipe{pipe}_vblank_err={bad}")
os.close(fd)
