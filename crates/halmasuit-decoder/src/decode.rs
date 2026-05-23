//! rsmpeg-driven video-decode helpers for `halmasuit-decoder`
//! (Epic #12).
//!
//! Receives a wallpaper file fd from the compositor via SCM_RIGHTS,
//! mmaps it into the decoder's address space, drives libavformat
//! through a custom-callback AVIO context whose read/seek read from
//! a cursor over the mmap'd slice, finds the best video stream,
//! opens the codec, and decodes RGBA8 frames via `SwsContext`.
//!
//! ## Why a custom AVIO, not `/dev/fd/N` path open
//!
//! Earlier versions opened the wallpaper via the `/dev/fd/N` pseudo-
//! path. That path's open semantics on Linux give the new file
//! struct the EXISTING fd's position (effectively a dup-like share),
//! so after one full decode pass the shared position sat at EOF and
//! every re-open immediately reported zero packets to libavformat —
//! the loop-on-EOF path livelocked at "open succeeded, read returned
//! EOF, repeat" forever (Epic #12 task #24). `seek_to_pts(0) +
//! avcodec_flush_buffers` also failed to clear AVIO's stuck
//! `eof_reached` flag on short MP4 inputs.
//!
//! The mmap + custom-AVIO design sidesteps both problems:
//! - The mmap is in the decoder's address space; no kernel fd-
//!   position state is shared with anything.
//! - On EOF + loop the decoder calls [`rewind`], which drops the
//!   AVFormatContext + AVIO and rebuilds them from the SAME mmap
//!   with a fresh cursor at offset 0. libavformat sees a brand-
//!   new input every loop iteration.
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

/// Internal AVSEEK_SIZE flag — when libavformat passes this in
/// `whence`, the seek callback must return the total stream size
/// (not perform a seek). Mirrors the C `#define AVSEEK_SIZE 0x10000`.
const AVSEEK_SIZE: i32 = 0x10000;

/// AVIO read-buffer size handed to `avio_alloc_context`. libavformat
/// reads ahead into this buffer; 4 KiB is the standard FFmpeg default
/// and plenty for our streaming-from-memory case.
const AVIO_READ_BUFFER_BYTES: usize = 4 * 1024;

/// One RGBA8 frame extracted from the decoder. Caller (the IPC
/// driver in main.rs) owns the bytes and writes them to the wire.
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
    pub bytes: Vec<u8>,
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
            if whence == AVSEEK_SIZE {
                return total;
            }
            let base = match whence {
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
/// convert to RGBA8 via `SwsContext`. Returns `Ok(None)` when the
/// stream is at EOF (after the decoder has been fully drained).
pub fn decode_next_frame(state: &mut DecoderState) -> Result<Option<RgbaFrame>, DecodeError> {
    loop {
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
            Ok(frame) => return convert_frame_to_rgba(&frame, state.time_base).map(Some),
            // Need more packets; loop iterates naturally.
            Err(RsmpegError::DecoderDrainError) => {}
            Err(err) => return Err(DecodeError::Codec(err)),
        }
    }
}

/// Drain ONE frame from the codec after EOF flush. Returns
/// `Ok(None)` if the codec is fully drained (we've delivered every
/// frame and `read_packet` is now hitting EOF).
fn drain_one_frame(state: &mut DecoderState) -> Result<Option<RgbaFrame>, DecodeError> {
    match state.dec.receive_frame() {
        Ok(frame) => convert_frame_to_rgba(&frame, state.time_base).map(Some),
        Err(RsmpegError::DecoderFlushedError) => Ok(None),
        Err(err) => Err(DecodeError::Codec(err)),
    }
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

fn convert_frame_to_rgba(
    frame: &rsmpeg::avutil::AVFrame,
    time_base: ffi::AVRational,
) -> Result<RgbaFrame, DecodeError> {
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

    // Set up sws_scale: src format → RGBA8. SWS_BILINEAR is a fine
    // default for the wallpaper use case (sharper algorithms cost
    // CPU we don't need for a decorative surface).
    let mut sws = SwsContext::get_context(
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

    let mut dst = rsmpeg::avutil::AVFrame::new();
    dst.set_width(frame.width);
    dst.set_height(frame.height);
    dst.set_format(ffi::AV_PIX_FMT_RGBA);
    dst.alloc_buffer().map_err(DecodeError::Codec)?;
    sws.scale_frame(frame, 0, frame.height, &mut dst)
        .map_err(DecodeError::SwsScale)?;

    // Extract RGBA bytes. AV_PIX_FMT_RGBA is interleaved RGBA8888,
    // tightly packed (linesize == width * 4) when alloc_buffer
    // produces it. Validate the linesize matches.
    let expected_linesize = (width * 4) as i32;
    let actual_linesize = dst.linesize[0];
    let row_stride = if actual_linesize == expected_linesize {
        expected_linesize as usize
    } else {
        // Padded; copy row-by-row.
        actual_linesize as usize
    };
    let mut bytes = Vec::with_capacity(expected_bytes as usize);
    // SAFETY: dst.data[0] points to a buffer of linesize[0] * height
    // bytes (libavutil's documented contract); we read it as a slice
    // of that length and copy out the tightly-packed RGBA.
    #[expect(
        unsafe_code,
        reason = "dst.data[0] is a libavutil-owned buffer of linesize[0] * height bytes; we slice it for one read-only copy."
    )]
    unsafe {
        let src_ptr = dst.data[0];
        let height_usize = frame.height as usize;
        let tight_row = (width * 4) as usize;
        for row in 0..height_usize {
            let row_start = src_ptr.add(row * row_stride);
            let row_slice = std::slice::from_raw_parts(row_start, tight_row);
            bytes.extend_from_slice(row_slice);
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
        bytes,
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
