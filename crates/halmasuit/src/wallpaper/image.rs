// halmasuit/src/wallpaper/image.rs — static-image wallpaper backend.
//
// The "today's PNG path" refactored behind the
// [`WallpaperBackend`](super::WallpaperBackend) trait. Behavior-
// identical to the prior in-renderer code: the file is decoded once
// at construction time into a tightly-packed RGBA8 `TextureBuffer`,
// and every frame's `render_element` rebuilds the cheap
// `TextureRenderElement` view of that same buffer scaled to the
// output. The decode happens synchronously during `new`, satisfying
// the frame-0 readiness invariant the no-flash gate depends on
// (epic G1/R3/R6).

use std::io;
use std::path::Path;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use super::backend::WallpaperBackend;
use crate::drm::SceneElement;

/// Decode the wallpaper image file at `path` into tightly-packed
/// RGBA8 bytes (`[R, G, B, A]` per pixel — the layout
/// `Fourcc::Abgr8888` reads in little-endian order) plus its pixel
/// dimensions.
fn decode(path: &Path) -> io::Result<(Vec<u8>, i32, i32)> {
    let bytes = std::fs::read(path)
        .map_err(|e| io::Error::other(format!("read wallpaper {}: {e}", path.display())))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| io::Error::other(format!("decode wallpaper {}: {e}", path.display())))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let w = i32::try_from(w).map_err(|_| {
        io::Error::other(format!(
            "wallpaper {} width {w} exceeds i32::MAX",
            path.display()
        ))
    })?;
    let h = i32::try_from(h).map_err(|_| {
        io::Error::other(format!(
            "wallpaper {} height {h} exceeds i32::MAX",
            path.display()
        ))
    })?;
    Ok((img.into_raw(), w, h))
}

/// Static-image wallpaper backend.
///
/// Owns the GPU-imported texture and its logical pixel size. The
/// size is load-bearing: `TextureRenderElement::from_texture_buffer`
/// with `src = None` defaults the source to the DESTINATION logical
/// size (the full output), NOT the texture's extent — which makes
/// the sampler read far outside the texture and clamp every pixel to
/// the edge texel (a uniform smear of the bottom-right pixel). We
/// pass an explicit `src` of exactly this texture rectangle so the
/// whole image is sampled and stretched to fill the output.
pub struct ImageBackend {
    buffer: TextureBuffer<GlesTexture>,
    size: Size<i32, Logical>,
}

impl ImageBackend {
    /// Decode the wallpaper file at `path` into a GPU texture and
    /// build an `ImageBackend`. The decode is synchronous (frame-0
    /// readiness — every frame the renderer composites after this
    /// returns is wallpaper-covered).
    ///
    /// # Errors
    ///
    /// Bubbles file-read, image-decode, or GPU-texture-import
    /// failure with context.
    pub fn new(renderer: &mut GlesRenderer, path: &Path) -> io::Result<Self> {
        let (rgba, w, h) = decode(path)?;
        let buffer = TextureBuffer::from_memory(
            renderer,
            &rgba,
            Fourcc::Abgr8888,
            (w, h),
            false,
            1,
            Transform::Normal,
            None,
        )
        .map_err(|e| io::Error::other(format!("wallpaper texture import: {e}")))?;
        // scale=1, transform=Normal ⇒ logical size == pixel size.
        Ok(Self {
            buffer,
            size: Size::from((w, h)),
        })
    }
}

impl WallpaperBackend for ImageBackend {
    fn render_element(
        &mut self,
        _renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        // Destination = the full output (stretch, no aspect
        // preservation). Source = the wallpaper texture's OWN extent
        // (NOT None — see the struct doc): smithay scales src→dst,
        // so this samples the whole image and stretches it to fill.
        let src = Rectangle::<f64, Logical>::from_size(self.size.to_f64());
        Ok(SceneElement::Wallpaper(
            TextureRenderElement::from_texture_buffer(
                Point::<f64, smithay::utils::Physical>::from((0.0, 0.0)),
                &self.buffer,
                None,
                Some(src),
                Some(output_size),
                Kind::Unspecified,
            ),
        ))
    }
}
