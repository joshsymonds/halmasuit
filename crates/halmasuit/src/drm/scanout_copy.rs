// halmasuit/src/drm/scanout_copy.rs — the render-node split's scan-out
// presenter. With `HALMASUIT_RENDER_NODE` set, every output is rendered
// with GLES on the render node and presented on the scan-out device by
// copying the frame to the CPU and flipping it in mmap'd dumb buffers
// on that output's CRTC. The scan-out device cannot import the render
// GPU's buffers (vkms), so no GPU buffer ever crosses the device boundary.
//
// The pure decisions (the pixel conversion from the readback to a pitched
// XRGB8888 buffer, and the flip scheduling state machine) are free of DRM
// and GL and are exercised directly by the unit tests. The DRM and GL I/O
// (dumb buffers, the offscreen texture, the readback, the flip) sits on
// top of them in [`CopyScanout`].

use std::io;

use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::{DrmDeviceFd, DrmSurface, PlaneConfig, PlaneState};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
use smithay::output::Output;
use smithay::reexports::drm::buffer::Buffer as DrmBuffer;
use smithay::reexports::drm::buffer::DrmFourcc;
use smithay::reexports::drm::control::Device as ControlDevice;
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::framebuffer;
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};

/// Convert a tightly packed, top-down `[R, G, B, A]` readback of
/// `width` x `height` pixels into a pitched XRGB8888 (`[B, G, R, X]`,
/// little-endian) buffer `dst` whose rows are `pitch` bytes apart.
///
/// Only the `width * 4` pixel bytes of each row are written. The
/// `pitch - width * 4` padding bytes after each row are left untouched.
///
/// # Errors
///
/// Returns an error if the size is zero, if `rgba` is not exactly
/// `width * height * 4` bytes, if `pitch` cannot hold a row, or if `dst`
/// is shorter than `pitch * height`.
pub fn rgba_to_xrgb8888(
    rgba: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    dst: &mut [u8],
) -> io::Result<()> {
    let row_bytes = width
        .checked_mul(4)
        .filter(|&n| n > 0 && height > 0)
        .ok_or_else(|| io::Error::other("scan-out copy: zero-sized frame"))?;
    let src_len = row_bytes
        .checked_mul(height)
        .ok_or_else(|| io::Error::other("scan-out copy: frame size overflows"))?;
    if rgba.len() != src_len {
        return Err(io::Error::other(
            "scan-out copy: readback is not width * height * 4 bytes",
        ));
    }
    if pitch < row_bytes {
        return Err(io::Error::other(
            "scan-out copy: pitch is shorter than one pixel row",
        ));
    }
    let dst_len = pitch
        .checked_mul(height)
        .ok_or_else(|| io::Error::other("scan-out copy: pitch * height overflows"))?;
    if dst.len() < dst_len {
        return Err(io::Error::other(
            "scan-out copy: destination is shorter than pitch * height",
        ));
    }
    for (dst_row, src_row) in dst
        .chunks_mut(pitch)
        .zip(rgba.chunks_exact(row_bytes))
        .take(height)
    {
        let (pixels, _padding) = dst_row.split_at_mut(row_bytes);
        for (d, s) in pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src_row.as_chunks::<4>().0)
        {
            *d = [s[2], s[1], s[0], 0];
        }
    }
    Ok(())
}

/// The inverse of [`rgba_to_xrgb8888`] for a flipped buffer: reads a
/// pitched XRGB8888 image of `width` x `height` pixels, `pitch` bytes per
/// row, and returns it as tightly packed top-down `[R, G, B, 0xFF]` pixels.
/// The row padding is ignored.
///
/// # Errors
///
/// Returns an error if the size is zero, if `pitch` cannot hold a row, or
/// if `src` is shorter than `pitch * height`.
#[cfg(feature = "frame_audit")]
pub fn xrgb8888_to_rgba(
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
) -> io::Result<Vec<u8>> {
    let row_bytes = width
        .checked_mul(4)
        .filter(|&n| n > 0 && height > 0)
        .ok_or_else(|| io::Error::other("scan-out copy: zero-sized frame"))?;
    if pitch < row_bytes {
        return Err(io::Error::other(
            "scan-out copy: pitch is shorter than one pixel row",
        ));
    }
    let src_len = pitch
        .checked_mul(height)
        .ok_or_else(|| io::Error::other("scan-out copy: pitch * height overflows"))?;
    if src.len() < src_len {
        return Err(io::Error::other(
            "scan-out copy: buffer is shorter than pitch * height",
        ));
    }
    let mut rgba = Vec::with_capacity(row_bytes * height);
    for src_row in src.chunks_exact(pitch).take(height) {
        let (pixels, _padding) = src_row.split_at(row_bytes);
        for px in pixels.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
        }
    }
    Ok(rgba)
}

/// The seq a presentation carries, for frame_audit's recorder. A flip
/// presents the texture as it stands, so the frame that reaches the
/// screen carries the seq of the latest damaged render whose content it
/// holds: its own seq when presented at once, or the latest deferred
/// render's seq when presented at the completion of the flip in flight.
#[cfg(feature = "frame_audit")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SeqBook {
    deferred: Option<u64>,
}

#[cfg(feature = "frame_audit")]
impl SeqBook {
    /// A render with `seq` was given `action`. Returns the seq of the frame
    /// presented now, if any. A deferred render replaces any earlier one.
    pub const fn frame_rendered(&mut self, action: FrameAction, seq: u64) -> Option<u64> {
        match action {
            FrameAction::Nothing => None,
            FrameAction::Present => Some(seq),
            FrameAction::Defer => {
                self.deferred = Some(seq);
                None
            }
        }
    }

    /// The flip in flight completed. Returns the seq of the deferred frame
    /// it presents, if one was waiting.
    pub const fn flip_completed(&mut self) -> Option<u64> {
        self.deferred.take()
    }
}

/// What a rendered frame asks the scan-out path to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// The scene had no damage: nothing is queued for scan-out.
    Nothing,
    /// No flip is in flight: present the frame now.
    Present,
    /// A flip is in flight: the frame is presented when that flip completes.
    Defer,
}

/// What a flip completion asks the scan-out path to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipCompletion {
    /// No frame was waiting: nothing is presented.
    Idle,
    /// A frame was deferred during the flip: present it now.
    PresentPending,
}

/// Flip scheduling for one output. At most one flip is in flight, and at
/// most one rendered frame waits behind it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlipScheduler {
    in_flight: bool,
    pending: bool,
}

impl FlipScheduler {
    /// A frame was rendered; `damaged` is whether the scene changed.
    pub const fn frame_rendered(&mut self, damaged: bool) -> FrameAction {
        if !damaged {
            FrameAction::Nothing
        } else if self.in_flight {
            self.pending = true;
            FrameAction::Defer
        } else {
            self.in_flight = true;
            FrameAction::Present
        }
    }

    /// The in-flight flip completed (its vblank arrived).
    pub fn flip_completed(&mut self) -> FlipCompletion {
        self.in_flight = false;
        if std::mem::take(&mut self.pending) {
            self.in_flight = true;
            FlipCompletion::PresentPending
        } else {
            FlipCompletion::Idle
        }
    }

    /// A flip could not be submitted. Nothing is in flight and nothing waits.
    pub fn flip_failed(&mut self) {
        *self = Self::default();
    }
}

/// One mmap'd XRGB8888 dumb buffer on the scan-out device, with the
/// framebuffer that scans it out. Both are released on drop.
struct DumbFramebuffer {
    fd: DrmDeviceFd,
    /// `Option` so `Drop` can hand the buffer to `destroy_dumb_buffer`,
    /// which consumes it. Always `Some` outside `Drop`.
    buffer: Option<DumbBuffer>,
    fb: framebuffer::Handle,
    pitch: u32,
    size: (u16, u16),
}

impl DumbFramebuffer {
    fn new(fd: &DrmDeviceFd, size: (u16, u16)) -> io::Result<Self> {
        let buffer = fd.create_dumb_buffer(
            (u32::from(size.0), u32::from(size.1)),
            DrmFourcc::Xrgb8888,
            32,
        )?;
        let fb = match fd.add_framebuffer(&buffer, 24, 32) {
            Ok(fb) => fb,
            Err(e) => {
                let _ = fd.destroy_dumb_buffer(buffer);
                return Err(e);
            }
        };
        let pitch = buffer.pitch();
        Ok(Self {
            fd: fd.clone(),
            buffer: Some(buffer),
            fb,
            pitch,
            size,
        })
    }

    /// Convert `rgba` (a `size` readback) into this buffer's mapping.
    fn write_rgba(&mut self, rgba: &[u8]) -> io::Result<()> {
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        let pitch = usize::try_from(self.pitch)
            .map_err(|_| io::Error::other("scan-out copy: pitch overflows usize"))?;
        let buffer = self
            .buffer
            .as_mut()
            .ok_or_else(|| io::Error::other("scan-out copy: dumb buffer already released"))?;
        let mut mapping = self.fd.map_dumb_buffer(buffer)?;
        rgba_to_xrgb8888(rgba, w, h, pitch, &mut mapping)
    }
}

#[cfg(feature = "frame_audit")]
impl DumbFramebuffer {
    /// The buffer's pixels as packed top-down `[R, G, B, 0xFF]`.
    fn read_rgba(&mut self) -> io::Result<Vec<u8>> {
        let (w, h) = (usize::from(self.size.0), usize::from(self.size.1));
        let pitch = usize::try_from(self.pitch)
            .map_err(|_| io::Error::other("scan-out copy: pitch overflows usize"))?;
        let buffer = self
            .buffer
            .as_mut()
            .ok_or_else(|| io::Error::other("scan-out copy: dumb buffer already released"))?;
        let mapping = self.fd.map_dumb_buffer(buffer)?;
        xrgb8888_to_rgba(&mapping, w, h, pitch)
    }
}

impl Drop for DumbFramebuffer {
    fn drop(&mut self) {
        let _ = self.fd.destroy_framebuffer(self.fb);
        if let Some(buffer) = self.buffer.take() {
            let _ = self.fd.destroy_dumb_buffer(buffer);
        }
    }
}

/// One output's render-node split presenter. GL renders the scene into
/// `texture` on the render node. A flip presents that texture by reading
/// it back into one of two dumb buffers on the scan-out device, and the
/// other buffer is never on screen or in flight when it is written.
pub struct CopyScanout {
    surface: DrmSurface,
    size: (u16, u16),
    /// The render target, `Abgr8888` at the output's mode size. It holds
    /// the most recent rendered scene until the next render replaces it.
    texture: GlesTexture,
    damage: OutputDamageTracker,
    /// `true` once a render has run. Until then the damage tracker
    /// repaints fully (age 0); afterwards it diffs against the texture
    /// (age 1).
    rendered: bool,
    buffers: [DumbFramebuffer; 2],
    /// Index of the buffer the next flip reads into. Presents happen only
    /// while idle, or at the completion of the in-flight flip, so the
    /// buffer at this index is never the one on screen or in flight.
    back: usize,
    scheduler: FlipScheduler,
    /// The flip accepted since the last [`Self::take_presented`]. Only
    /// exists in `halmasuit-debug`, for the frame recorder.
    #[cfg(feature = "frame_audit")]
    flipped: Option<Flipped>,
}

/// A flip the scan-out device accepted: the buffer it scans out and the
/// monotonic time it was accepted.
#[cfg(feature = "frame_audit")]
struct Flipped {
    index: usize,
    mono_us: u64,
}

/// A frame the scan-out device accepted, as the frame recorder takes it.
#[cfg(feature = "frame_audit")]
pub struct Presented {
    /// `CLOCK_MONOTONIC` when the flip was accepted.
    pub mono_us: u64,
    pub width: u32,
    pub height: u32,
    /// The scanned-out pixels as packed `[R, G, B, 0xFF]`, or why they could
    /// not be read back.
    pub rgba: io::Result<Vec<u8>>,
}

impl CopyScanout {
    /// # Errors
    ///
    /// Returns an error if a dumb buffer, its framebuffer, or the offscreen
    /// texture cannot be created.
    pub fn new(
        renderer: &mut GlesRenderer,
        surface: DrmSurface,
        output: &Output,
        size: (u16, u16),
    ) -> io::Result<Self> {
        let fd = surface.device_fd().clone();
        let buffers = [
            DumbFramebuffer::new(&fd, size)?,
            DumbFramebuffer::new(&fd, size)?,
        ];
        let texture = Offscreen::<GlesTexture>::create_buffer(
            renderer,
            Fourcc::Abgr8888,
            Size::from((i32::from(size.0), i32::from(size.1))),
        )
        .map_err(|e| io::Error::other(format!("scan-out copy texture: {e}")))?;
        Ok(Self {
            surface,
            size,
            texture,
            damage: OutputDamageTracker::from_output(output),
            rendered: false,
            buffers,
            back: 0,
            scheduler: FlipScheduler::default(),
            #[cfg(feature = "frame_audit")]
            flipped: None,
        })
    }

    /// The scan-out device this output flips on.
    pub fn device_fd(&self) -> &DrmDeviceFd {
        self.surface.device_fd()
    }

    /// Render `elements` over `clear_color` into the texture. Returns
    /// `Ok(true)` when the scene had damage, whether it was presented now
    /// or deferred behind the flip in flight. `Ok(false)` means nothing was
    /// queued.
    ///
    /// # Errors
    ///
    /// Returns an error if the bind, the render, or the flip fails.
    pub fn render<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[E],
        clear_color: Color32F,
    ) -> io::Result<bool> {
        let age = usize::from(self.rendered);
        let mut target = Bind::bind(renderer, &mut self.texture)
            .map_err(|e| io::Error::other(format!("scan-out copy bind: {e}")))?;
        let damaged = self
            .damage
            .render_output(renderer, &mut target, age, elements, clear_color)
            .map_err(|e| io::Error::other(format!("scan-out copy render_output: {e:?}")))?
            .damage
            .is_some();
        // The target borrows the texture, which `submit` needs next.
        drop(target);
        self.rendered = true;
        match self.scheduler.frame_rendered(damaged) {
            FrameAction::Nothing => Ok(false),
            FrameAction::Defer => Ok(true),
            FrameAction::Present => {
                self.submit(renderer)?;
                Ok(true)
            }
        }
    }

    /// The in-flight flip completed. If a frame was deferred behind it,
    /// present that frame now.
    ///
    /// # Errors
    ///
    /// Returns an error if the deferred frame cannot be read back or flipped.
    pub fn frame_submitted(&mut self, renderer: &mut GlesRenderer) -> io::Result<()> {
        match self.scheduler.flip_completed() {
            FlipCompletion::Idle => Ok(()),
            FlipCompletion::PresentPending => self.submit(renderer),
        }
    }

    /// The flip accepted since the last call, with its pixels read back from
    /// the buffer it scanned out. Only the frame recorder calls this, so
    /// without a recorder nothing is read back.
    #[cfg(feature = "frame_audit")]
    pub fn take_presented(&mut self) -> Option<Presented> {
        let flipped = self.flipped.take()?;
        let (w, h) = self.size;
        Some(Presented {
            mono_us: flipped.mono_us,
            width: u32::from(w),
            height: u32::from(h),
            rgba: self.buffers[flipped.index].read_rgba(),
        })
    }

    /// Present the texture on the scan-out device. A failed submission
    /// clears the scheduler so the next damaged frame presents again.
    fn submit(&mut self, renderer: &mut GlesRenderer) -> io::Result<()> {
        let result = self.flip_back(renderer);
        if result.is_err() {
            self.scheduler.flip_failed();
        }
        result
    }

    fn flip_back(&mut self, renderer: &mut GlesRenderer) -> io::Result<()> {
        let index = self.back;
        let buffer = &mut self.buffers[index];
        readback(renderer, &mut self.texture, buffer)?;
        let (w, h) = self.size;
        let plane = PlaneState {
            handle: self.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size(Size::<f64, Buffer>::from((f64::from(w), f64::from(h)))),
                dst: Rectangle::from_size(Size::<i32, Physical>::from((
                    i32::from(w),
                    i32::from(h),
                ))),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: buffer.fb,
                fence: None,
            }),
        };
        let flip = if self.surface.commit_pending() {
            self.surface.commit([plane], true)
        } else {
            self.surface.page_flip([plane], true)
        };
        flip.map_err(|e| io::Error::other(format!("scan-out copy flip: {e}")))?;
        #[cfg(feature = "frame_audit")]
        {
            self.flipped = Some(Flipped {
                index,
                mono_us: crate::recorder::monotonic_us(),
            });
        }
        self.back = 1 - index;
        Ok(())
    }
}

/// Read the texture's current scene back as `[R, G, B, A]` and write it
/// into `dst` as XRGB8888.
fn readback(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    dst: &mut DumbFramebuffer,
) -> io::Result<()> {
    let (w, h) = dst.size;
    let region = Rectangle::from_size(Size::from((i32::from(w), i32::from(h))));
    let mapping = {
        let fb = Bind::bind(renderer, texture)
            .map_err(|e| io::Error::other(format!("scan-out copy bind: {e}")))?;
        ExportMem::copy_framebuffer(renderer, &fb, region, Fourcc::Abgr8888)
            .map_err(|e| io::Error::other(format!("scan-out copy readback: {e}")))?
    };
    let rgba = ExportMem::map_texture(renderer, &mapping)
        .map_err(|e| io::Error::other(format!("scan-out copy map: {e}")))?;
    dst.write_rgba(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rgba_rows_to_bgrx_and_keeps_row_padding() {
        // 2x2 image, pitch 16: each 8-byte pixel row is followed by 8 bytes
        // of padding that must stay at the 0xEE sentinel.
        #[rustfmt::skip]
        let rgba = [
            0x0A, 0x00, 0x14, 0xFF,  0xFF, 0x00, 0x00, 0xFF, // row 0: #0a0014, red
            0x00, 0xFF, 0x00, 0xFF,  0x00, 0x00, 0xFF, 0xFF, // row 1: green, blue
        ];
        let mut dst = vec![0xEE; 16 * 2];
        rgba_to_xrgb8888(&rgba, 2, 2, 16, &mut dst).expect("valid geometry");
        #[rustfmt::skip]
        let expected = [
            0x14, 0x00, 0x0A, 0x00,  0x00, 0x00, 0xFF, 0x00, // row 0
            0xEE, 0xEE, 0xEE, 0xEE,  0xEE, 0xEE, 0xEE, 0xEE, // row 0 padding
            0x00, 0xFF, 0x00, 0x00,  0xFF, 0x00, 0x00, 0x00, // row 1
            0xEE, 0xEE, 0xEE, 0xEE,  0xEE, 0xEE, 0xEE, 0xEE, // row 1 padding
        ];
        assert_eq!(dst, expected);
    }

    #[test]
    fn packed_rows_without_padding_convert_in_place_order() {
        // pitch == width * 4: no padding, so the output is the packed image.
        let rgba = [0x01, 0x02, 0x03, 0xFF, 0x04, 0x05, 0x06, 0x80];
        let mut dst = vec![0; 8];
        rgba_to_xrgb8888(&rgba, 2, 1, 8, &mut dst).expect("valid geometry");
        assert_eq!(dst, [0x03, 0x02, 0x01, 0x00, 0x06, 0x05, 0x04, 0x00]);
    }

    #[test]
    fn rejects_geometry_that_does_not_fit_the_buffers() {
        let mut dst = vec![0; 64];
        assert!(
            rgba_to_xrgb8888(&[0; 7], 2, 1, 8, &mut dst).is_err(),
            "source shorter than width * height * 4"
        );
        assert!(
            rgba_to_xrgb8888(&[0; 8], 2, 1, 4, &mut dst).is_err(),
            "pitch below one pixel row"
        );
        assert!(
            rgba_to_xrgb8888(&[0; 16], 2, 2, 8, &mut [0; 15]).is_err(),
            "destination shorter than pitch * height"
        );
        assert!(
            rgba_to_xrgb8888(&[], 0, 0, 0, &mut dst).is_err(),
            "zero-sized frame"
        );
    }

    #[test]
    fn idle_damaged_frame_presents_immediately() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
    }

    #[test]
    fn undamaged_frame_queues_nothing_idle_or_in_flight() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(false), FrameAction::Nothing);
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
        assert_eq!(sched.frame_rendered(false), FrameAction::Nothing);
        assert_eq!(
            sched.flip_completed(),
            FlipCompletion::Idle,
            "an undamaged frame during a flip must not have been deferred"
        );
    }

    #[test]
    fn damaged_frame_during_a_flip_is_deferred() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
        assert_eq!(sched.frame_rendered(true), FrameAction::Defer);
    }

    #[test]
    fn deferred_frame_presents_on_completion() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
        assert_eq!(sched.frame_rendered(true), FrameAction::Defer);
        assert_eq!(sched.flip_completed(), FlipCompletion::PresentPending);
        // The deferred frame is now in flight, so the next one defers again.
        assert_eq!(sched.frame_rendered(true), FrameAction::Defer);
    }

    #[test]
    fn completion_without_a_waiting_frame_presents_nothing() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
        assert_eq!(sched.flip_completed(), FlipCompletion::Idle);
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
    }

    #[test]
    fn failed_flip_leaves_the_scheduler_idle() {
        let mut sched = FlipScheduler::default();
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
        assert_eq!(sched.frame_rendered(true), FrameAction::Defer);
        sched.flip_failed();
        assert_eq!(sched.flip_completed(), FlipCompletion::Idle);
        assert_eq!(sched.frame_rendered(true), FrameAction::Present);
    }

    #[cfg(feature = "frame_audit")]
    #[test]
    fn pitched_xrgb_reads_back_as_packed_rgba_ignoring_row_padding() {
        // The inverse of `converts_rgba_rows_to_bgrx_and_keeps_row_padding`'s
        // image: pitch 16, 8 padding bytes per row, alpha forced to 0xFF.
        #[rustfmt::skip]
        let xrgb = [
            0x14, 0x00, 0x0A, 0x00,  0x00, 0x00, 0xFF, 0x00, // row 0
            0xEE, 0xEE, 0xEE, 0xEE,  0xEE, 0xEE, 0xEE, 0xEE, // row 0 padding
            0x00, 0xFF, 0x00, 0x00,  0xFF, 0x00, 0x00, 0x00, // row 1
            0xEE, 0xEE, 0xEE, 0xEE,  0xEE, 0xEE, 0xEE, 0xEE, // row 1 padding
        ];
        let rgba = xrgb8888_to_rgba(&xrgb, 2, 2, 16).expect("valid geometry");
        #[rustfmt::skip]
        let expected = [
            0x0A, 0x00, 0x14, 0xFF,  0xFF, 0x00, 0x00, 0xFF, // row 0: #0a0014, red
            0x00, 0xFF, 0x00, 0xFF,  0x00, 0x00, 0xFF, 0xFF, // row 1: green, blue
        ];
        assert_eq!(rgba, expected);
    }

    #[cfg(feature = "frame_audit")]
    #[test]
    fn pitched_xrgb_readback_rejects_geometry_that_does_not_fit() {
        assert!(
            xrgb8888_to_rgba(&[0; 15], 2, 2, 8).is_err(),
            "source shorter than pitch * height"
        );
        assert!(
            xrgb8888_to_rgba(&[0; 8], 2, 1, 4).is_err(),
            "pitch below one pixel row"
        );
        assert!(xrgb8888_to_rgba(&[], 0, 0, 0).is_err(), "zero-sized frame");
    }

    #[cfg(feature = "frame_audit")]
    #[test]
    fn presented_now_carries_its_own_seq() {
        let mut book = SeqBook::default();
        assert_eq!(book.frame_rendered(FrameAction::Present, 7), Some(7));
        assert_eq!(book.flip_completed(), None);
    }

    #[cfg(feature = "frame_audit")]
    #[test]
    fn deferred_frame_superseded_before_completion_presents_once_with_later_seq() {
        let mut book = SeqBook::default();
        assert_eq!(book.frame_rendered(FrameAction::Present, 1), Some(1));
        assert_eq!(book.frame_rendered(FrameAction::Defer, 2), None);
        assert_eq!(book.frame_rendered(FrameAction::Defer, 3), None);
        assert_eq!(book.flip_completed(), Some(3));
        assert_eq!(book.flip_completed(), None, "presented once");
    }

    #[cfg(feature = "frame_audit")]
    #[test]
    fn undamaged_render_changes_nothing() {
        let mut book = SeqBook::default();
        assert_eq!(book.frame_rendered(FrameAction::Present, 1), Some(1));
        assert_eq!(book.frame_rendered(FrameAction::Defer, 2), None);
        assert_eq!(book.frame_rendered(FrameAction::Nothing, 9), None);
        assert_eq!(book.flip_completed(), Some(2), "seq 2 still pending");
    }
}
