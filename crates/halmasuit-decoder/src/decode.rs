//! rsmpeg-driven video-decode helpers for `halmasuit-decoder`
//! (Epic #12).
//!
//! Receives a wallpaper file fd from the compositor via SCM_RIGHTS,
//! mmaps it into the decoder's address space, drives libavformat
//! through a custom-callback AVIO context whose read/seek read from
//! a cursor over the mmap'd slice, finds the best video stream,
//! opens the codec, and decodes RGBA8 frames via `SwsContext`.
//!
//! ## mmap + custom AVIO
//!
//! The mmap lives in the decoder's address space; no kernel fd-
//! position state is shared with anything outside the decoder
//! process. On EOF + loop the decoder calls [`rewind`], which drops
//! the AVFormatContext + AVIO and rebuilds them from the SAME mmap
//! with cursor at offset 0 — libavformat sees a brand-new input
//! every loop iteration.
//!
//! ## Phase A scope
//!
//! - h264 + AV1 codecs (FFmpeg's stock libavcodec; AV1 via libdav1d
//!   when the system FFmpeg was built with `--enable-libdav1d`,
//!   which `pkgs.ffmpeg-headless` is).
//! - Up to 1080p RGBA8 output (`MAX_FRAME_BYTES = 16 MiB`).
//! - Wallpaper file size bounded by [`MAX_WALLPAPER_BYTES`] — we
//!   mmap the entire file, so very large inputs would balloon the
//!   decoder's VSZ past `RLIMIT_AS` (512 MiB).
//!
//! ## Phase B (deferred)
//!
//! - Hardware decode via VAAPI → dmabuf zero-copy.
//! - Higher-than-1080p resolutions (shared-memory pool replaces the
//!   single-datagram model).
//! - Pixel-format negotiation for YUV passthrough (avoid sws_scale
//!   when the compositor can sample YUV directly).

// reason: rsmpeg's FFI surface returns C `int` for sizes and uses
// signed integers for dimensions; converting to/from unsigned/usize
// at every cast site is mechanical noise that obscures the FFI shape.
// We bound dimensions via MAX_FRAME_BYTES validation, which makes the
// truncation/sign casts safe in this module's context.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::os::fd::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex};

use memmap2::Mmap;
use rsmpeg::avcodec::AVCodecContext;
use rsmpeg::avformat::{AVFormatContextInput, AVIOContextContainer, AVIOContextCustom};
use rsmpeg::avutil::AVMem;
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;
use thiserror::Error;
use tracing::info;

/// Hard ceiling on the wallpaper file size we'll mmap. The decoder's
/// RLIMIT_AS is 512 MiB; we leave room for the codec's working set
/// (typically <100 MiB for 1080p h264/AV1) + the libav* libraries.
/// Files larger than this fail [`open_video_input`] with
/// [`DecodeError::WallpaperTooLarge`].
const MAX_WALLPAPER_BYTES: u64 = 256 * 1024 * 1024;

/// AVIO read-buffer size handed to `avio_alloc_context`. libavformat
/// reads ahead into this buffer; 4 KiB is the standard FFmpeg default
/// and plenty for our streaming-from-memory case.
const AVIO_READ_BUFFER_BYTES: usize = 4 * 1024;

/// One RGBA8 frame extracted from the decoder. The `bytes` slice
/// borrows from a reusable scratch buffer owned by
/// [`DecoderState`]; the caller (the IPC driver in main.rs) reads
/// the slice and writes it to the wire, and the borrow ends when
/// the frame drops at end-of-iteration. This avoids the per-frame
/// `Vec<u8>` allocation (8.3 MiB at 1080p × ~30 fps = ~250 MiB/s
/// of allocator churn) that the previous owned-bytes shape caused.
pub struct RgbaFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
    pub bytes: &'a [u8],
}

/// State held between [`open_video_input`] / [`rewind`] and the
/// per-frame decode calls in main.rs.
pub struct DecoderState {
    ictx: AVFormatContextInput,
    dec: AVCodecContext,
    /// Index of the video stream within `ictx.streams()`.
    stream_idx: usize,
    /// AV time base of the video stream; used to convert
    /// `frame.pts` → microseconds.
    time_base: ffi::AVRational,
    /// The mmap'd wallpaper file, retained so [`rewind`] can rebuild
    /// the AVFormatContext + AVIO over the same memory region
    /// without re-reading the underlying file. `Arc` because both
    /// the read and seek AVIO callbacks need a borrow.
    wallpaper: Arc<Mmap>,
    /// Cached sws_scale context for the steady-state input → RGBA
    /// conversion. Built lazily on first `convert_frame_to_rgba`,
    /// rebuilt if the input dimensions or pixel format change. Saves
    /// ~50–200 µs per frame vs. constructing fresh each call.
    sws: Option<SwsContextCacheEntry>,
    /// Cached destination AVFrame (RGBA buffer). Reused across
    /// frames at fixed dimensions to avoid an 8 MiB libavutil alloc
    /// per 1080p frame.
    dst: Option<DstFrameCacheEntry>,
    /// Reusable RGBA scratch buffer. `convert_frame_to_rgba` clears
    /// it and fills with the tight-packed RGBA bytes; the returned
    /// `RgbaFrame` borrows the slice. Reusing the buffer eliminates
    /// the per-frame 8 MiB Vec alloc that previously dominated
    /// allocator churn.
    rgba_scratch: Vec<u8>,
}

/// Cache key + value for the sws_scale context. Recreated when any
/// input parameter changes.
struct SwsContextCacheEntry {
    width: i32,
    height: i32,
    src_fmt: i32,
    ctx: SwsContext,
}

/// Cached destination frame for sws_scale. Recreated when dimensions
/// change.
struct DstFrameCacheEntry {
    width: i32,
    height: i32,
    frame: rsmpeg::avutil::AVFrame,
}

/// Errors from the rsmpeg-driven decode path. Mapped to
/// [`halmasuit_decoder_ipc::DecoderErrorCode`] before sending on
/// the wire.
#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("invalid wallpaper fd {0} for /dev/fd path conversion")]
    InvalidFd(RawFd),
    #[error("libavformat open failed: {0}")]
    OpenFailed(RsmpegError),
    #[error("no video stream found in input")]
    NoVideoStream,
    #[error("unsupported codec id {0:?}")]
    UnsupportedCodec(ffi::AVCodecID),
    #[error("libavcodec error: {0}")]
    Codec(RsmpegError),
    #[error("sws_scale context allocation failed")]
    SwsAllocFailed,
    #[error("sws_scale conversion failed: {0}")]
    SwsScale(RsmpegError),
    #[error("decoded frame too large: {width}x{height} ({bytes} bytes) > MAX_FRAME_BYTES")]
    FrameTooLarge { width: u32, height: u32, bytes: u64 },
    #[error("wallpaper file too large: {bytes} bytes > MAX_WALLPAPER_BYTES ({max} bytes)")]
    WallpaperTooLarge { bytes: u64, max: u64 },
    #[error("wallpaper file mmap failed: {0}")]
    MmapFailed(std::io::Error),
}

impl DecodeError {
    /// Map to a stable wire code for `DecoderToCompositor::DecoderError`.
    pub const fn to_wire_code(&self) -> halmasuit_decoder_ipc::DecoderErrorCode {
        use halmasuit_decoder_ipc::DecoderErrorCode as W;
        match self {
            Self::InvalidFd(_) => W::Internal,
            Self::OpenFailed(_) | Self::NoVideoStream => W::OpenFailed,
            Self::UnsupportedCodec(_) => W::UnsupportedCodec,
            Self::Codec(_) | Self::SwsScale(_) => W::ParseError,
            Self::SwsAllocFailed
            | Self::FrameTooLarge { .. }
            | Self::WallpaperTooLarge { .. }
            | Self::MmapFailed(_) => W::AllocationFailed,
        }
    }
}

/// Build a custom AVIO context that reads + seeks over the
/// `wallpaper` mmap. The cursor is held in a `Mutex<usize>` shared
/// between the read and seek closures (rsmpeg's `Send + 'static`
/// bound on the callback types rules out `Cell` / `Rc`); the lock
/// is uncontested in practice — libavformat invokes the callbacks
/// sequentially on the decoder's single thread.
fn build_avio(wallpaper: &Arc<Mmap>) -> AVIOContextCustom {
    let cursor = Arc::new(Mutex::new(0usize));

    let wallpaper_r = Arc::clone(wallpaper);
    let cursor_r = Arc::clone(&cursor);
    let read_cb: rsmpeg::avformat::ReadPacketCallback =
        Box::new(move |_opaque: &mut Vec<u8>, buf: &mut [u8]| -> i32 {
            let Ok(mut c) = cursor_r.lock() else {
                return ffi::AVERROR_EXTERNAL;
            };
            let avail = wallpaper_r.len().saturating_sub(*c);
            if avail == 0 {
                return ffi::AVERROR_EOF;
            }
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&wallpaper_r[*c..*c + n]);
            *c += n;
            i32::try_from(n).unwrap_or(i32::MAX)
        });

    let wallpaper_s = Arc::clone(wallpaper);
    let cursor_s = Arc::clone(&cursor);
    let seek_cb: rsmpeg::avformat::SeekCallback = Box::new(
        move |_opaque: &mut Vec<u8>, offset: i64, whence: i32| -> i64 {
            let Ok(mut c) = cursor_s.lock() else {
                return -1;
            };
            let total = wallpaper_s.len() as i64;
            // libavformat's AVSEEK_SIZE asks for the total stream
            // size; do NOT move the cursor on this whence value.
            // AVSEEK_FORCE may be OR'd into whence by some protocol
            // paths; strip it before matching the base.
            let avseek_size = ffi::AVSEEK_SIZE as i32;
            let avseek_force = ffi::AVSEEK_FORCE as i32;
            if whence == avseek_size {
                return total;
            }
            let base = match whence & !avseek_force {
                libc::SEEK_SET => 0,
                libc::SEEK_CUR => *c as i64,
                libc::SEEK_END => total,
                _ => return -1,
            };
            let Some(new_pos) = base.checked_add(offset) else {
                return -1;
            };
            if new_pos < 0 || new_pos > total {
                return -1;
            }
            *c = new_pos as usize;
            new_pos
        },
    );

    let buffer = AVMem::new(AVIO_READ_BUFFER_BYTES);
    AVIOContextCustom::alloc_context(
        buffer,
        false, // read mode
        Vec::new(),
        Some(read_cb),
        None,
        Some(seek_cb),
    )
}

/// Build a fresh `AVFormatContextInput` over a given wallpaper mmap.
/// Called by [`open_video_input`] (initial open) and [`rewind`]
/// (loop-on-EOF restart). Each call produces a brand-new
/// AVFormatContext + AVIO with cursor at offset 0; libavformat
/// never sees any state from a previous iteration.
fn build_state(wallpaper: Arc<Mmap>) -> Result<DecoderState, DecodeError> {
    let avio = build_avio(&wallpaper);
    let ictx = AVFormatContextInput::builder()
        .io_context(AVIOContextContainer::Custom(avio))
        .open()
        .map_err(DecodeError::OpenFailed)?;

    let (stream_idx, decoder) = ictx
        .find_best_stream(ffi::AVMEDIA_TYPE_VIDEO)
        .map_err(DecodeError::Codec)?
        .ok_or(DecodeError::NoVideoStream)?;

    // Accept h264 and AV1; reject anything else (Phase A scope).
    let codec_id = decoder.id;
    if codec_id != ffi::AV_CODEC_ID_H264 && codec_id != ffi::AV_CODEC_ID_AV1 {
        return Err(DecodeError::UnsupportedCodec(codec_id));
    }

    let mut dec = AVCodecContext::new(&decoder);
    dec.apply_codecpar(&ictx.streams()[stream_idx].codecpar())
        .map_err(DecodeError::Codec)?;
    dec.open(None).map_err(DecodeError::Codec)?;

    let time_base = ictx.streams()[stream_idx].time_base;

    Ok(DecoderState {
        ictx,
        dec,
        stream_idx,
        time_base,
        wallpaper,
        sws: None,
        dst: None,
        rgba_scratch: Vec::new(),
    })
}

/// Open the wallpaper file at `fd` (received via SCM_RIGHTS from
/// the compositor): dup the fd, mmap the underlying file once, and
/// drive libavformat through a custom-callback AVIO context over
/// the mmap'd slice.
///
/// The dup ensures we own a private fd whose drop closes correctly
/// without disturbing the caller's fd table. The mmap pins the file
/// for the decoder's lifetime; subsequent [`rewind`] calls reuse
/// the SAME mmap without touching the fd again.
///
/// # Errors
///
/// - [`DecodeError::InvalidFd`] if `dup` fails (EMFILE / EBADF).
/// - [`DecodeError::WallpaperTooLarge`] if the file exceeds
///   [`MAX_WALLPAPER_BYTES`].
/// - [`DecodeError::MmapFailed`] if `mmap(2)` rejects the file
///   (non-regular, sealed, etc.).
/// - Plus the usual rsmpeg errors from `build_state`.
pub fn open_video_input(fd: RawFd) -> Result<DecoderState, DecodeError> {
    // Dup the fd so the Mmap owns its own File handle independent
    // of the caller's fd table.
    #[expect(
        unsafe_code,
        reason = "dup gives us a fresh fd we own; from_raw_fd takes ownership of it"
    )]
    let file = {
        let raw = unsafe { libc::dup(fd) };
        if raw < 0 {
            return Err(DecodeError::InvalidFd(fd));
        }
        unsafe { std::fs::File::from_raw_fd(raw) }
    };

    let metadata = file.metadata().map_err(DecodeError::MmapFailed)?;
    let size = metadata.len();
    if size > MAX_WALLPAPER_BYTES {
        return Err(DecodeError::WallpaperTooLarge {
            bytes: size,
            max: MAX_WALLPAPER_BYTES,
        });
    }

    // SAFETY: the file is a private dup of the wallpaper fd; nothing
    // else holds a handle to it. memmap2's Mmap::map(&file) requires
    // unsafe because the OS file could be modified externally — for
    // wallpaper files that's a non-issue (the operator wouldn't
    // mutate a wallpaper mid-playback; even if they did, the worst
    // case is libavformat seeing corrupted bytes and the relay's
    // restart-budget machinery kicks in).
    #[expect(
        unsafe_code,
        reason = "Mmap::map's unsafety is about the file being externally mutated; \
                  wallpaper files aren't mutated mid-playback, and corruption is \
                  handled by the relay's restart budget"
    )]
    let mmap = unsafe { Mmap::map(&file) }.map_err(DecodeError::MmapFailed)?;
    drop(file);
    let wallpaper = Arc::new(mmap);

    let state = build_state(Arc::clone(&wallpaper))?;
    info!(
        bytes = wallpaper.len(),
        codec = ?state.dec.codec_id,
        width = state.dec.width,
        height = state.dec.height,
        "decoder: opened video input (mmap'd)",
    );
    Ok(state)
}

/// Rewind for loop-on-EOF: drop the current AVFormatContext + AVIO
/// and build a fresh pair over the SAME wallpaper mmap with cursor
/// at offset 0. Cheaper than re-reading the file (no syscalls), and
/// sidesteps both the `/dev/fd/N`-position bug and the stuck-AVIO-
/// EOF bug — libavformat sees a brand-new input.
pub fn rewind(state: &mut DecoderState) -> Result<(), DecodeError> {
    let wallpaper = Arc::clone(&state.wallpaper);
    *state = build_state(wallpaper)?;
    info!("decoder: rewound for loop");
    Ok(())
}

/// Read packets and decode until the next video frame emerges;
/// convert to RGBA8 via `SwsContext`. The returned `RgbaFrame`
/// borrows `state.rgba_scratch`; the borrow ends when the frame
/// drops. Returns `Ok(None)` when the stream is at EOF (after the
/// decoder has been fully drained).
pub fn decode_next_frame(state: &mut DecoderState) -> Result<Option<RgbaFrame<'_>>, DecodeError> {
    // Two-phase: receive an AVFrame into a local, THEN convert. We
    // can't `return convert(state, &frame)` from inside the match
    // arm because `state.dec.receive_frame()` borrows `state.dec`
    // for the duration of the match expression, and convert needs
    // `state: &mut`. The owned AVFrame outlives the borrow.
    let frame = loop {
        let packet = state.ictx.read_packet().map_err(DecodeError::Codec)?;
        let Some(packet) = packet else {
            // EOF: flush the decoder and drain any pending frames.
            state.dec.send_packet(None).map_err(DecodeError::Codec)?;
            return drain_one_frame(state);
        };
        if (packet.stream_index as usize) != state.stream_idx {
            continue;
        }
        state
            .dec
            .send_packet(Some(&packet))
            .map_err(DecodeError::Codec)?;
        match state.dec.receive_frame() {
            Ok(f) => break f,
            // Need more packets; loop iterates naturally.
            Err(RsmpegError::DecoderDrainError) => {}
            Err(err) => return Err(DecodeError::Codec(err)),
        }
    };
    convert_frame_to_rgba(state, &frame).map(Some)
}

/// Drain ONE frame from the codec after EOF flush. Returns
/// `Ok(None)` if the codec is fully drained (we've delivered every
/// frame and `read_packet` is now hitting EOF).
fn drain_one_frame(state: &mut DecoderState) -> Result<Option<RgbaFrame<'_>>, DecodeError> {
    let frame = match state.dec.receive_frame() {
        Ok(f) => f,
        Err(RsmpegError::DecoderFlushedError) => return Ok(None),
        Err(err) => return Err(DecodeError::Codec(err)),
    };
    convert_frame_to_rgba(state, &frame).map(Some)
}

/// Seek the format context to `pts_us` (microseconds). Flushes the
/// codec so the next `decode_next_frame` returns a freshly-decoded
/// frame at the new position.
pub fn seek_to_pts(state: &mut DecoderState, pts_us: i64) -> Result<(), DecodeError> {
    // Convert microseconds → stream's time_base units.
    // ts_units = pts_us * time_base.den / (time_base.num * 1_000_000)
    let num = i128::from(state.time_base.num).max(1);
    let den = i128::from(state.time_base.den).max(1);
    let pts_units = i64::try_from(i128::from(pts_us) * den / (num * 1_000_000)).unwrap_or(0);
    let stream_idx_i32 = i32::try_from(state.stream_idx).unwrap_or(-1);
    state
        .ictx
        .seek(stream_idx_i32, pts_units, ffi::AVSEEK_FLAG_BACKWARD as i32)
        .map_err(DecodeError::Codec)?;
    state.dec.flush_buffers();
    info!(pts_us, "decoder: sought to pts");
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "linear convert pipeline: validate size → sws cache check → dst cache check → scale → copy out → pts conversion. Splitting would scatter the AVFrame lifetime across helpers."
)]
fn convert_frame_to_rgba<'a>(
    state: &'a mut DecoderState,
    frame: &rsmpeg::avutil::AVFrame,
) -> Result<RgbaFrame<'a>, DecodeError> {
    let time_base = state.time_base;
    let width = u32::try_from(frame.width).unwrap_or(0);
    let height = u32::try_from(frame.height).unwrap_or(0);
    let src_fmt = frame.format;
    let bytes_per_pixel: u64 = 4;
    let expected_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|wh| wh.checked_mul(bytes_per_pixel))
        .ok_or(DecodeError::FrameTooLarge {
            width,
            height,
            bytes: u64::MAX,
        })?;
    if expected_bytes > u64::from(halmasuit_decoder_ipc::MAX_FRAME_BYTES) {
        return Err(DecodeError::FrameTooLarge {
            width,
            height,
            bytes: expected_bytes,
        });
    }

    // sws_scale context (reused across calls for fixed dimensions —
    // rebuilt only on a dimension or pixel-format change).
    // SWS_BILINEAR is a fine default for the wallpaper use case
    // (sharper algorithms cost CPU we don't need for a decorative
    // surface).
    let needs_new_sws = state
        .sws
        .as_ref()
        .is_none_or(|e| e.width != frame.width || e.height != frame.height || e.src_fmt != src_fmt);
    if needs_new_sws {
        let ctx = SwsContext::get_context(
            frame.width,
            frame.height,
            src_fmt,
            frame.width,
            frame.height,
            ffi::AV_PIX_FMT_RGBA,
            ffi::SWS_BILINEAR,
            None,
            None,
            None,
        )
        .ok_or(DecodeError::SwsAllocFailed)?;
        state.sws = Some(SwsContextCacheEntry {
            width: frame.width,
            height: frame.height,
            src_fmt,
            ctx,
        });
    }

    // Destination AVFrame (reused across calls for fixed dimensions).
    let needs_new_dst = state
        .dst
        .as_ref()
        .is_none_or(|d| d.width != frame.width || d.height != frame.height);
    if needs_new_dst {
        let mut new_dst = rsmpeg::avutil::AVFrame::new();
        new_dst.set_width(frame.width);
        new_dst.set_height(frame.height);
        new_dst.set_format(ffi::AV_PIX_FMT_RGBA);
        new_dst.alloc_buffer().map_err(DecodeError::Codec)?;
        state.dst = Some(DstFrameCacheEntry {
            width: frame.width,
            height: frame.height,
            frame: new_dst,
        });
    }

    let dst_entry = state.dst.as_mut().expect("dst cache populated above");
    let sws_entry = state.sws.as_mut().expect("sws cache populated above");
    sws_entry
        .ctx
        .scale_frame(frame, 0, frame.height, &mut dst_entry.frame)
        .map_err(DecodeError::SwsScale)?;

    // Extract RGBA bytes. AV_PIX_FMT_RGBA is interleaved RGBA8888,
    // tightly packed (linesize == width * 4) when alloc_buffer
    // produces it. If padded, copy row-by-row.
    let expected_linesize = (width * 4) as i32;
    let actual_linesize = dst_entry.frame.linesize[0];
    let row_stride = if actual_linesize == expected_linesize {
        expected_linesize as usize
    } else {
        actual_linesize as usize
    };
    let expected_usize = expected_bytes as usize;
    // Reuse state.rgba_scratch: clear preserves capacity; the
    // single reserve+extend below is the only allocation, and only
    // on first use or dimension change (capacity grows monotonically
    // up to the largest frame seen).
    state.rgba_scratch.clear();
    state.rgba_scratch.reserve(expected_usize);
    // SAFETY: dst.data[0] points to a buffer of linesize[0] * height
    // bytes (libavutil's documented contract); we read it as a slice
    // of that length and copy out the tightly-packed RGBA.
    #[expect(
        unsafe_code,
        reason = "dst.data[0] is a libavutil-owned buffer of linesize[0] * height bytes; we slice it for one read-only copy."
    )]
    unsafe {
        let src_ptr = dst_entry.frame.data[0];
        let height_usize = frame.height as usize;
        let tight_row = (width * 4) as usize;
        for row in 0..height_usize {
            let row_start = src_ptr.add(row * row_stride);
            let row_slice = std::slice::from_raw_parts(row_start, tight_row);
            state.rgba_scratch.extend_from_slice(row_slice);
        }
    }

    // Convert PTS (in time_base units) → microseconds.
    let pts_us = if frame.pts == ffi::AV_NOPTS_VALUE {
        0
    } else {
        // pts_us = pts * time_base.num / time_base.den * 1_000_000
        // Use 128-bit arithmetic to avoid overflow.
        let pts = i128::from(frame.pts);
        let num = i128::from(time_base.num);
        let den = i128::from(time_base.den).max(1);
        i64::try_from(pts * num * 1_000_000 / den).unwrap_or(0)
    };

    Ok(RgbaFrame {
        width,
        height,
        pts_us,
        bytes: &state.rgba_scratch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_decoder_ipc::DecoderErrorCode;

    #[test]
    fn decode_error_maps_to_wire_codes() {
        assert_eq!(
            DecodeError::InvalidFd(42).to_wire_code(),
            DecoderErrorCode::Internal,
        );
        assert_eq!(
            DecodeError::NoVideoStream.to_wire_code(),
            DecoderErrorCode::OpenFailed,
        );
        assert_eq!(
            DecodeError::UnsupportedCodec(ffi::AV_CODEC_ID_HEVC).to_wire_code(),
            DecoderErrorCode::UnsupportedCodec,
        );
        assert_eq!(
            DecodeError::SwsAllocFailed.to_wire_code(),
            DecoderErrorCode::AllocationFailed,
        );
        assert_eq!(
            DecodeError::FrameTooLarge {
                width: 4096,
                height: 4096,
                bytes: 4096 * 4096 * 4,
            }
            .to_wire_code(),
            DecoderErrorCode::AllocationFailed,
        );
    }

    // NOTE: end-to-end decode of a real h264 file is exercised in
    // the VM test (Epic #12 task #9). Unit tests here cover the
    // pure-Rust adapter layer (error mapping, format math); the
    // rsmpeg / libavcodec calls themselves need a real video
    // stream which we generate fresh in the VM test environment.
}
