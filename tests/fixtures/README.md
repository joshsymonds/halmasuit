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
