//! rsmpeg-driven video-decode helpers for `halmasuit-decoder`
//! (Epic #12 task #5).
//!
//! Receives a wallpaper file fd from the compositor via SCM_RIGHTS,
//! opens it via libavformat by formatting `/dev/fd/N` (kernel-level
//! fd-to-path), finds the best video stream, opens the codec, and
//! decodes the first frame into RGBA8 via `SwsContext`.
//!
//! ## Phase A scope
//!
//! - One frame per `LoadFile` (the multi-frame loop, Pause/Resume/
//!   Seek/EndOfFile lands in the next subtask, T18).
//! - h264 + AV1 codecs (FFmpeg's stock libavcodec; AV1 via libdav1d
//!   when the system FFmpeg was built with `--enable-libdav1d`,
//!   which `pkgs.ffmpeg-headless` is).
//! - Up to 1080p RGBA8 output (`MAX_FRAME_BYTES = 16 MiB`).
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

use std::ffi::CString;
use std::os::fd::RawFd;

use rsmpeg::avcodec::AVCodecContext;
use rsmpeg::avformat::AVFormatContextInput;
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;
use thiserror::Error;
use tracing::info;

/// One RGBA8 frame extracted from the decoder. Caller (the IPC
/// driver in main.rs) owns the bytes and writes them to the wire.
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
    pub bytes: Vec<u8>,
}

/// State held between `open_video_input` and `decode_first_frame`
/// (and, in T18, the multi-frame decode loop).
pub struct DecoderState {
    ictx: AVFormatContextInput,
    dec: AVCodecContext,
    /// Index of the video stream within `ictx.streams()`.
    stream_idx: usize,
    /// AV time base of the video stream; used to convert
    /// `frame.pts` → microseconds.
    time_base: ffi::AVRational,
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
            Self::SwsAllocFailed | Self::FrameTooLarge { .. } => W::AllocationFailed,
        }
    }
}

/// Open the wallpaper file at `fd` (received via SCM_RIGHTS from
/// the compositor) and prepare the decoder.
pub fn open_video_input(fd: RawFd) -> Result<DecoderState, DecodeError> {
    // /dev/fd/N is a kernel-provided pseudo-path that resolves to
    // the underlying file behind fd N. libavformat treats it as any
    // other path.
    let path = CString::new(format!("/dev/fd/{fd}")).map_err(|_| DecodeError::InvalidFd(fd))?;
    let ictx = AVFormatContextInput::open(&path).map_err(DecodeError::OpenFailed)?;

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

    info!(
        codec = ?codec_id,
        width = dec.width,
        height = dec.height,
        "decoder: opened video input",
    );

    Ok(DecoderState {
        ictx,
        dec,
        stream_idx,
        time_base,
    })
}

/// Read packets and decode until the first video frame emerges;
/// convert to RGBA8 via `SwsContext`.
pub fn decode_first_frame(state: &mut DecoderState) -> Result<RgbaFrame, DecodeError> {
    loop {
        let packet = state.ictx.read_packet().map_err(DecodeError::Codec)?;
        let Some(packet) = packet else {
            // EOF before first frame: flush the decoder.
            state.dec.send_packet(None).map_err(DecodeError::Codec)?;
            return drain_first_frame(state);
        };
        if (packet.stream_index as usize) != state.stream_idx {
            continue;
        }
        state
            .dec
            .send_packet(Some(&packet))
            .map_err(DecodeError::Codec)?;
        match state.dec.receive_frame() {
            Ok(frame) => return convert_frame_to_rgba(&frame, state.time_base),
            // Need more packets; loop iterates naturally.
            Err(RsmpegError::DecoderDrainError) => {}
            Err(err) => return Err(DecodeError::Codec(err)),
        }
    }
}

fn drain_first_frame(state: &mut DecoderState) -> Result<RgbaFrame, DecodeError> {
    match state.dec.receive_frame() {
        Ok(frame) => convert_frame_to_rgba(&frame, state.time_base),
        Err(err) => Err(DecodeError::Codec(err)),
    }
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
