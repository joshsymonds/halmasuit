// halmasuit/src/recorder.rs — frame_audit-only lossless frame recorder.
//
// Entirely `#[cfg(feature = "frame_audit")]` (the `mod` line in main.rs
// is gated). Compiled into `halmasuit-debug`, never into the production
// `halmasuit` binary.
//
// With `HALMASUIT_RECORD_FRAMES=<dir>` set, every frame the render path
// presents on a `Scanout::Compositor` output is written losslessly as
// `<dir>/<output>/<seq:08>.png` (8-bit RGB), with one JSON line per
// written frame appended to `<dir>/index.jsonl`.
//
// Submitting never blocks the render thread: `submit` is a `try_send`
// onto a bounded queue drained by a writer thread that owns all PNG
// encoding and file IO. A frame refused by a full queue, or one the
// render path could not capture (`skip`), is logged at warn and counted;
// the count rides on the next written index line as `dropped`. A frame
// whose output name or write fails inside the writer is also logged and
// counted.
//
// The writer resolves every path relative to directory fds opened at
// `start`, never by re-resolving `<dir>`: the process root changes across
// switch-root and chroot while frames are being written.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::OwnedFd;
use std::path::{Component, Path};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::{Mode, fstat, mkdirat};
use nix::unistd::{Gid, Uid, fchown};

/// Queue depth between the render thread and the writer thread.
const QUEUE_CAPACITY: usize = 8;

/// Mode of every frame file and the index file.
const FILE_MODE: Mode = Mode::from_bits_truncate(0o644);

/// Mode of every output directory (before the owner is applied).
const DIR_MODE: Mode = Mode::from_bits_truncate(0o755);

/// One captured frame: tightly-packed top-down `[R, G, B, A]` pixels.
pub struct Frame {
    pub output: String,
    pub seq: u64,
    pub mono_us: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A frame queued for the writer. `dropped_before` is the number of
/// frames refused on the submit side (full queue or `skip`) before this
/// one; the writer adds its own refusals to produce the index `dropped`.
struct Job {
    frame: Frame,
    dropped_before: u64,
}

/// Render-thread handle to the recorder. Dropping it closes the queue
/// and joins the writer after it drains.
pub struct Recorder {
    tx: Option<SyncSender<Job>>,
    writer: Option<JoinHandle<()>>,
    submit_dropped: u64,
}

impl Recorder {
    /// Recorder from `HALMASUIT_RECORD_FRAMES`. `None` when the variable
    /// is unset (no thread is started) or the recorder cannot start.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var_os("HALMASUIT_RECORD_FRAMES")?;
        match Self::start(Path::new(&dir), QUEUE_CAPACITY) {
            Ok(recorder) => Some(recorder),
            Err(e) => {
                tracing::error!(
                    dir = %Path::new(&dir).display(),
                    error = %e,
                    "frame recorder not started"
                );
                None
            }
        }
    }

    /// Create `dir` if missing, open it, and start the writer thread with
    /// a queue of `capacity` frames.
    ///
    /// # Errors
    ///
    /// Returns an error if `dir` cannot be created or opened, the index
    /// file cannot be opened, or the writer thread cannot be spawned.
    pub fn start(dir: &Path, capacity: usize) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let dir_fd = open(
            dir,
            OFlag::O_DIRECTORY | OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fstat(&dir_fd)?;
        let owner = (Uid::from_raw(stat.st_uid), Gid::from_raw(stat.st_gid));
        let index = File::from(openat(
            &dir_fd,
            c"index.jsonl",
            OFlag::O_WRONLY
                | OFlag::O_CREAT
                | OFlag::O_APPEND
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC,
            FILE_MODE,
        )?);

        let (tx, rx) = mpsc::sync_channel(capacity);
        let writer = Writer {
            dir: dir_fd,
            index,
            owner,
            unwritten: 0,
        };
        let handle = std::thread::Builder::new()
            .name("frame-recorder".to_owned())
            .spawn(move || writer.run(&rx))?;
        Ok(Self {
            tx: Some(tx),
            writer: Some(handle),
            submit_dropped: 0,
        })
    }

    /// Queue `frame` for writing. Never blocks; a full queue refuses it.
    pub fn submit(&mut self, frame: Frame) {
        let job = Job {
            dropped_before: self.submit_dropped,
            frame,
        };
        let tx = self.tx.as_ref().expect("queue is open until drop");
        match tx.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                self.submit_dropped += 1;
                tracing::warn!(
                    output = %job.frame.output,
                    seq = job.frame.seq,
                    "frame recorder queue full; frame not recorded"
                );
            }
            Err(TrySendError::Disconnected(job)) => {
                self.submit_dropped += 1;
                tracing::warn!(
                    output = %job.frame.output,
                    seq = job.frame.seq,
                    "frame recorder writer is gone; frame not recorded"
                );
            }
        }
    }

    /// Count and log a presented frame that could not be captured.
    pub fn skip(&mut self, output: &str, seq: u64, error: &io::Error) {
        self.submit_dropped += 1;
        tracing::warn!(
            output,
            seq,
            error = %error,
            "frame not captured; frame not recorded"
        );
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.writer.take()
            && handle.join().is_err()
        {
            tracing::error!("frame recorder writer thread panicked");
        }
    }
}

/// `CLOCK_MONOTONIC` in microseconds, the `mono_us` timebase.
#[must_use]
pub fn monotonic_us() -> u64 {
    smithay::utils::Clock::<smithay::utils::Monotonic>::new()
        .now()
        .as_micros()
}

/// Owns every file descriptor the writer touches. All paths resolve
/// relative to `dir` or to an output directory fd derived from it.
struct Writer {
    dir: OwnedFd,
    index: File,
    /// Owner and group of `<dir>`, applied to each output directory the
    /// writer creates.
    owner: (Uid, Gid),
    /// Frames the writer failed to record. Added to the submit-side count
    /// on every index line.
    unwritten: u64,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Job>) {
        for job in rx {
            if let Err(e) = self.record(&job.frame) {
                self.unwritten += 1;
                tracing::warn!(
                    output = %job.frame.output,
                    seq = job.frame.seq,
                    error = %e,
                    "frame not recorded"
                );
                continue;
            }
            let line = index_line(&job.frame, job.dropped_before + self.unwritten);
            if let Err(e) = self.index.write_all(line.as_bytes()) {
                self.unwritten += 1;
                tracing::warn!(
                    output = %job.frame.output,
                    seq = job.frame.seq,
                    error = %e,
                    "frame index line not written"
                );
            }
        }
    }

    /// Write the PNG for `frame`. Runs before its index line is written.
    fn record(&self, frame: &Frame) -> io::Result<()> {
        let component = single_component(&frame.output)?;
        let rgb = rgb_bytes(frame)?;
        let out_dir = self.output_dir(component)?;
        let file_name = format!("{:08}.png", frame.seq);
        let file = File::from(openat(
            &out_dir,
            file_name.as_str(),
            OFlag::O_WRONLY
                | OFlag::O_CREAT
                | OFlag::O_TRUNC
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC,
            FILE_MODE,
        )?);
        write_png(file, frame.width, frame.height, &rgb)
    }

    /// Open the directory `component` under `<dir>`, creating it if
    /// missing. A directory the writer creates is handed to the owner of
    /// `<dir>`: the first frame is written as root, later frames as the
    /// compositor user.
    fn output_dir(&self, component: &str) -> io::Result<OwnedFd> {
        let flags = OFlag::O_DIRECTORY | OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        match mkdirat(&self.dir, component, DIR_MODE) {
            Ok(()) => {
                let fd = openat(&self.dir, component, flags, Mode::empty())?;
                fchown(&fd, Some(self.owner.0), Some(self.owner.1))?;
                Ok(fd)
            }
            Err(Errno::EEXIST) => Ok(openat(&self.dir, component, flags, Mode::empty())?),
            Err(e) => Err(e.into()),
        }
    }
}

/// `output` as a directory name, if it is exactly one normal path
/// component. Anything else (`..`, `.`, empty, absolute, multi-component,
/// embedded NUL) is refused, so no write can leave `<dir>`.
fn single_component(output: &str) -> io::Result<&str> {
    let mut parts = Path::new(output).components();
    let normal = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    );
    if normal && !output.contains(['/', '\0']) {
        Ok(output)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("output name {output:?} is not one path component"),
        ))
    }
}

/// `frame.rgba` with alpha dropped: tightly-packed `[R, G, B]` pixels.
fn rgb_bytes(frame: &Frame) -> io::Result<Vec<u8>> {
    let expected = usize::try_from(frame.width)
        .ok()
        .zip(usize::try_from(frame.height).ok())
        .and_then(|(w, h)| w.checked_mul(h))
        .and_then(|px| px.checked_mul(4));
    if expected != Some(frame.rgba.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "rgba length {} does not match {}x{}",
                frame.rgba.len(),
                frame.width,
                frame.height
            ),
        ));
    }
    let (pixels, _) = frame.rgba.as_chunks::<4>();
    Ok(pixels.iter().flat_map(|px| [px[0], px[1], px[2]]).collect())
}

fn write_png(file: File, width: u32, height: u32, rgb: &[u8]) -> io::Result<()> {
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|e| io::Error::other(format!("png header: {e}")))?;
    writer
        .write_image_data(rgb)
        .map_err(|e| io::Error::other(format!("png data: {e}")))?;
    writer
        .finish()
        .map_err(|e| io::Error::other(format!("png finish: {e}")))
}

fn index_line(frame: &Frame, dropped: u64) -> String {
    let line = serde_json::json!({
        "seq": frame.seq,
        "output": frame.output,
        "mono_us": frame.mono_us,
        "width": frame.width,
        "height": frame.height,
        "format": "rgb8",
        "dropped": dropped,
    });
    format!("{line}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;

    fn rgba_pattern(width: u32, height: u32, salt: usize) -> Vec<u8> {
        let len = usize::try_from(width).unwrap() * usize::try_from(height).unwrap() * 4;
        (0..len)
            .map(|i| u8::try_from((i * 31 + salt) % 256).unwrap())
            .collect()
    }

    fn frame(output: &str, seq: u64, mono_us: u64, width: u32, height: u32, salt: usize) -> Frame {
        Frame {
            output: output.to_owned(),
            seq,
            mono_us,
            width,
            height,
            rgba: rgba_pattern(width, height, salt),
        }
    }

    fn rgb_of(rgba: &[u8]) -> Vec<u8> {
        let (pixels, _) = rgba.as_chunks::<4>();
        pixels.iter().flat_map(|px| [px[0], px[1], px[2]]).collect()
    }

    fn decode_png(path: &Path) -> (png::ColorType, png::BitDepth, u32, u32, Vec<u8>) {
        let file = io::BufReader::new(std::fs::File::open(path).expect("open png"));
        let mut reader = png::Decoder::new(file).read_info().expect("png header");
        let mut buf = vec![0; reader.output_buffer_size().expect("buffer size")];
        let info = reader.next_frame(&mut buf).expect("png frame");
        buf.truncate(info.buffer_size());
        (
            info.color_type,
            info.bit_depth,
            info.width,
            info.height,
            buf,
        )
    }

    fn index_lines(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("index.jsonl"))
            .expect("index.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .into_string()
                    .expect("utf8")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn two_frames_round_trip_losslessly_with_exact_index_fields() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("rec");
        let first = frame("DP-1", 0, 1_000, 3, 2, 1);
        let second = frame("HDMI-A-1", 1, 2_000, 4, 1, 2);
        let expected_first = rgb_of(&first.rgba);
        let expected_second = rgb_of(&second.rgba);

        let mut rec = Recorder::start(&dir, QUEUE_CAPACITY).expect("start");
        rec.submit(first);
        rec.submit(second);
        drop(rec);

        let (ct, bd, w, h, px) = decode_png(&dir.join("DP-1").join("00000000.png"));
        assert_eq!(
            (ct, bd, w, h),
            (png::ColorType::Rgb, png::BitDepth::Eight, 3, 2)
        );
        assert_eq!(px, expected_first);

        let (ct, bd, w, h, px) = decode_png(&dir.join("HDMI-A-1").join("00000001.png"));
        assert_eq!(
            (ct, bd, w, h),
            (png::ColorType::Rgb, png::BitDepth::Eight, 4, 1)
        );
        assert_eq!(px, expected_second);

        assert_eq!(
            index_lines(&dir),
            vec![
                json!({"seq": 0, "output": "DP-1", "mono_us": 1000, "width": 3,
                       "height": 2, "format": "rgb8", "dropped": 0}),
                json!({"seq": 1, "output": "HDMI-A-1", "mono_us": 2000, "width": 4,
                       "height": 1, "format": "rgb8", "dropped": 0}),
            ]
        );
    }

    #[test]
    fn skipped_frame_is_counted_on_the_next_written_line() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("rec");

        let mut rec = Recorder::start(&dir, QUEUE_CAPACITY).expect("start");
        rec.submit(frame("DP-1", 0, 10, 2, 2, 3));
        rec.skip("DP-1", 1, &io::Error::other("readback failed"));
        rec.submit(frame("DP-1", 2, 30, 2, 2, 4));
        drop(rec);

        assert!(dir.join("DP-1").join("00000000.png").is_file());
        assert!(!dir.join("DP-1").join("00000001.png").exists());
        assert!(dir.join("DP-1").join("00000002.png").is_file());
        assert_eq!(
            index_lines(&dir),
            vec![
                json!({"seq": 0, "output": "DP-1", "mono_us": 10, "width": 2,
                       "height": 2, "format": "rgb8", "dropped": 0}),
                json!({"seq": 2, "output": "DP-1", "mono_us": 30, "width": 2,
                       "height": 2, "format": "rgb8", "dropped": 1}),
            ]
        );
    }

    #[test]
    fn full_queue_refuses_the_frame_and_counts_it_on_the_next_queued_job() {
        // No writer thread: the queue is observed directly, so the
        // full-queue refusal is deterministic.
        let (tx, rx) = mpsc::sync_channel(2);
        let mut rec = Recorder {
            tx: Some(tx),
            writer: None,
            submit_dropped: 0,
        };
        rec.submit(frame("DP-1", 0, 0, 1, 1, 0));
        rec.submit(frame("DP-1", 1, 0, 1, 1, 0));
        rec.submit(frame("DP-1", 2, 0, 1, 1, 0));

        let first = rx.recv().expect("first job");
        assert_eq!((first.frame.seq, first.dropped_before), (0, 0));
        let second = rx.recv().expect("second job");
        assert_eq!((second.frame.seq, second.dropped_before), (1, 0));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        rec.submit(frame("DP-1", 3, 0, 1, 1, 0));
        let fourth = rx.recv().expect("fourth job");
        assert_eq!((fourth.frame.seq, fourth.dropped_before), (3, 1));
    }

    #[test]
    fn unusable_output_names_are_refused_and_counted_without_leaving_the_directory() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("rec");
        let bad = ["..", ".", "", "a/b", "/abs", "../escape", "nul\0name"];

        let mut rec = Recorder::start(&dir, QUEUE_CAPACITY).expect("start");
        for (seq, name) in (0_u64..).zip(bad) {
            rec.submit(frame(name, seq, 0, 1, 1, 0));
        }
        rec.submit(frame("DP-1", 99, 5, 1, 1, 0));
        drop(rec);

        assert_eq!(
            index_lines(&dir),
            vec![
                json!({"seq": 99, "output": "DP-1", "mono_us": 5, "width": 1,
                        "height": 1, "format": "rgb8", "dropped": bad.len()})
            ]
        );
        assert_eq!(entries(base.path()), vec!["rec".to_owned()]);
        assert_eq!(
            entries(&dir),
            vec!["DP-1".to_owned(), "index.jsonl".to_owned()]
        );
    }
}
