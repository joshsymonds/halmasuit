# NVIDIA passthrough test rig (Epic #45) — operational notes

The `*-nvidia-*` / `visual-nvidia-*` tests run halmasuit (or bare probes)
against the **real RTX 5070 Ti** on `stygianlibrary` via single-GPU VFIO
passthrough. They are **RUNNER-ONLY** — never in `just test-vm` or GHA
(those stay hermetic + GPU-free). Run them with:

```
just test-vm-nvidia <test-name>     # builds on the dev host, nix-copies, runs on stygian
```

The card is the host's boot/primary GPU and is Blackwell, which makes the
rig touchy in two specific ways. Both are solved; this file is why.

## 1. Scanout needs a clean vBIOS via `romfile=` (else: black screen)

The 5070 Ti is the host's **boot/primary display** (`boot_vga=1`). Its
firmware POSTs the card, so a passthrough guest inherits a half-initialized
display engine and a **tainted** on-card vBIOS. Result: `nvidia-smi` and
EGL render work, KMS modeset "succeeds" (the monitor even wakes), but the
**actual scanout is black** — for every app, not just halmasuit (kmscube
is black too). The HW scanout CRC reads `0x0`.

Fix: pass the guest a **pristine UEFI vBIOS** via qemu `romfile=` (wired in
`tests/lib/nvidia-passthrough.nix`). The ROM on stygian is
`/home/joshsymonds/halmasuit-5070ti-vbios.rom` (166912 B, a `55aa`-aligned
x86+EFI option ROM). It was produced from a **GPU-Z / nvflash dump taken in
Windows** (the `/sys/.../rom` scrape of a `boot_vga` card is the post-POST
*shadowed* copy — it was missing two NVIDIA-ext images and did NOT work),
then trimmed to the option-ROM portion. No host BIOS change is needed; the
monitor stays on the 5070 Ti.

If the card is ever replaced, re-dump in Windows (GPU-Z "Save BIOS" → the
chip icon next to BIOS Version; or `nvflash64 --save`), extract the
`55aa`-aligned x86+EFI chain, drop it at that path.

## 2. Iterative runs need a GRACEFUL teardown (else: GPU wedges)

Blackwell has the well-known **VFIO reset bug**: once a guest has used the
GPU, hard-killing qemu leaves the GPU's GSP/WPR2 firmware state up, and it
**survives FLR, secondary bus reset, PCI remove/rescan, and driver
rebind** — there is **no software un-wedge**. A wedged card needs a host
**power-cycle** (and stygian boots Windows by default — manual presence).

So every NVIDIA test ends by sourcing `tests/lib/nvidia-teardown.sh`
(stop halmasuit → unload the nvidia modules while the guest is alive) and
then `machine.shutdown()` (clean ACPI poweroff). This quiesces the GPU so
the **next run works with no reboot**. Proven: 3 runs back-to-back, zero
reboots. (We can't use the upstream `nvidia-drm modeset=0` fix — halmasuit
requires KMS modeset — hence the explicit teardown.)

### Caveat: a FAILING test still wedges the GPU

The graceful teardown runs on the **success path**. If a testScript
**raises** (assertion failure, timeout, crash), the driver hard-kills qemu
before the teardown → the GPU wedges → you must **reboot stygian before the
next run**. This notably affects `visual-nvidia-greeter`, which is RED by
design (it reproduces the gnomon Quickshell buffer-attach bug). Hardening
each test with `try/finally` so the teardown also runs on failure is a
future improvement; for now, expect a reboot after a red run.

## Adding a new NVIDIA test

1. `imports = [ ./lib/nvidia-passthrough.nix ... ]` (brings the GPU,
   driver, EGL/GBM farm, and the `romfile`).
2. End the `testScript` with:
   ```python
   machine.execute("sh ${./lib/nvidia-teardown.sh}")
   machine.shutdown()
   ```
3. Register it in `flake.nix` under `checks.x86_64-linux`, RUNNER-ONLY.

## The flicker gate's signal (rung 4)

`visual-nvidia-flicker` reads the HW `compositorCrc32` per head (pre-dither,
frame-stable) via the patched `GET_CRTC_CRC32_V2` ioctl
(`tests/lib/crc-vblank-wait.patch`) and asserts, for a constant color:
non-zero (catches driver-drop), `distinct==1` (no flicker), and the two
different-resolution heads differ (content-sensitive — not a tautology).
The `rg`/`out` taps vary per frame from dithering and are not used for the
no-flicker check.
