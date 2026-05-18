# Test fixtures

## `splash-test.png`

A deterministic 256×256 four-colour quadrant image used by
`tests/visual-halmasuit-splash.nix` as `HALMASUIT_SPLASH_IMAGE`. It is
deliberately **non-uniform** so the resulting golden proves
halmasuit-splash actually textured the image, not that it fell back to
a clear colour.

Quadrants (top-left, top-right, bottom-left, bottom-right):

| quadrant | hex       | rgb           |
|----------|-----------|---------------|
| TL       | `#C81E1E` | (200, 30, 30) |
| TR       | `#1EC81E` | (30, 200, 30) |
| BL       | `#1E1EC8` | (30, 30, 200) |
| BR       | `#C8C81E` | (200, 200, 30)|

Regenerate exactly (ImageMagick 7 `magick`):

```sh
magick -size 128x128 xc:"#C81E1E" png:/tmp/q_tl.png
magick -size 128x128 xc:"#1EC81E" png:/tmp/q_tr.png
magick -size 128x128 xc:"#1E1EC8" png:/tmp/q_bl.png
magick -size 128x128 xc:"#C8C81E" png:/tmp/q_br.png
magick \( /tmp/q_tl.png /tmp/q_tr.png +append \) \
       \( /tmp/q_bl.png /tmp/q_br.png +append \) \
       -append -strip tests/fixtures/splash-test.png
```

halmasuit-splash stretches it to fill the output (v1 = no aspect
preservation), so at 1280×800 it still reads as four equal colour
quadrants — a stable, recognisable golden.

## `witness.png`

The locked **witness** image — the Ḫalmašuit (deified throne)
enthroned in the Hittite winged sun-disc: cobalt ground, gold/oxblood
constructivist screenprint. This is the *instrument* of the no-flash
continuity proof, not branding: it is the one image that must remain
on screen — never black, never moved, never resized, never replaced —
across every handover from frame 0 through the real DankGreeter, the
broker privilege drop, and into real niri. If at any captured phase
the pixels are not this image, the no-flash invariant has regressed.

- **2560×1600, 8-bit RGB, sRGB, no alpha.** 16:10; the render path
  stretches non-aspect-preserving, so it identity-stretches at 16:10
  outputs (e.g. 1280×800) with no distortion.
- It is **both** the ssimulacra2 reference the visual gates compare
  against **and** the source halmasuit composites as its internal
  witness plane (epic G-layer R3/R7).

Provenance: a generative composition, locked and human-approved
2026-05-18 (epic amendment G1). The generation master is kept **out
of the repo** — scratch/rejected/source generation images are never
committed (epic anti-pattern; repo permanence). If the master is
re-supplied, finalize it identically (ImageMagick 7):

```sh
magick <master>.png -resize '2560x1600!' -colorspace sRGB \
       -alpha off -strip tests/fixtures/witness.png
```

Regenerating this fixture changes every visual golden that references
it; it is therefore human-inspected before commit, never
CI-regenerated.
