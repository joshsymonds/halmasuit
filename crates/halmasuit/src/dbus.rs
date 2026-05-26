// halmasuit/src/dbus.rs — test-only D-Bus introspection surface.
//
// Entirely `#[cfg(feature = "frame_audit")]` (the `mod` line in
// main.rs is gated). Compiled into `halmasuit-debug`, never into the
// production `halmasuit` binary: `Snapshot()` is an arbitrary-file-
// write surface that does not belong in a privileged long-lived
// compositor (Epic #1 req 7 + anti-patterns).
//
// The single method `Snapshot(path)` lives ONLY on
// `org.halmasuit.Debug.Introspect`. It is deliberately NOT on
// `org.halmasuit.Compositor1` (which does not exist yet and must not
// grow this) — PLAN.md keeps the control plane and the observability
// plane on separate interfaces so scope cannot leak between them.
//
// The compositor's `GlesRenderer` is `!Send` and lives in the calloop
// thread, so the D-Bus object never touches it. Instead the render
// loop publishes the latest composited frame's RGBA into a shared
// `SnapshotBuffer`; the D-Bus method (served on its own executor
// thread by zbus) only reads that buffer and PNG-encodes it.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One captured frame: tightly-packed RGBA8 (`[R, G, B, A]` per pixel,
/// the byte order `ExportMem` yields for `Fourcc::Abgr8888`), plus its
/// dimensions.
#[derive(Clone)]
pub struct FrameBuf {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Shared latest-frame slot. The render loop overwrites it every
/// audited frame; `Snapshot()` reads the current value. `None` until
/// the first frame is composited.
pub type SnapshotBuffer = Arc<Mutex<Option<FrameBuf>>>;

/// Fresh, empty snapshot slot. Held by `DrmBackend` (publisher) and
/// handed to [`serve`] (reader); both sides hold clones of the `Arc`.
#[must_use]
pub fn new_buffer() -> SnapshotBuffer {
    Arc::new(Mutex::new(None))
}

/// PNG-encode one frame to `path`. RGBA8, 8-bit, no compression
/// tuning — correctness over size; these are test goldens.
///
/// # Errors
///
/// Returns an error if `frame.rgba` is too short for its dimensions,
/// or if the file cannot be created / written.
fn write_png(frame: &FrameBuf, path: &Path) -> io::Result<()> {
    let expected = frame
        .width
        .checked_mul(frame.height)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| io::Error::other("snapshot: frame dimensions overflow"))?;
    if frame.rgba.len() < expected {
        return Err(io::Error::other(format!(
            "snapshot: rgba {} bytes < {expected} for {}x{}",
            frame.rgba.len(),
            frame.width,
            frame.height
        )));
    }
    let w =
        u32::try_from(frame.width).map_err(|_| io::Error::other("snapshot: width exceeds u32"))?;
    let h = u32::try_from(frame.height)
        .map_err(|_| io::Error::other("snapshot: height exceeds u32"))?;

    let file = std::fs::File::create(path)?;
    let bufw = io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(bufw, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| io::Error::other(format!("snapshot: png header: {e}")))?;
    writer
        .write_image_data(&frame.rgba[..expected])
        .map_err(|e| io::Error::other(format!("snapshot: png data: {e}")))?;
    writer
        .finish()
        .map_err(|e| io::Error::other(format!("snapshot: png finish: {e}")))?;
    Ok(())
}

/// Pair of snapshot slots the D-Bus server can read from. `current`
/// holds the full composited frame (wallpaper + layers + foreground +
/// cursor); `wallpaper_only` holds an auxiliary capture of just the
/// wallpaper element (no cursor, no layer-shell, no xdg-toplevel) so
/// the Phase B golden gates can distinguish wallpaper variants even
/// when niri's fullscreen toplevel covers the wallpaper in the live
/// composition.
#[derive(Clone)]
pub struct SnapshotHandles {
    pub current: SnapshotBuffer,
    pub wallpaper_only: SnapshotBuffer,
}

/// The `org.halmasuit.Debug.Introspect` D-Bus object.
pub struct Introspect {
    bufs: SnapshotHandles,
}

impl Introspect {
    /// Core of the `Snapshot` method, factored out so it is unit-
    /// testable without a live bus: copy the current frame out from
    /// under the lock, then PNG-encode it to `path`. `scene` selects
    /// the slot (`"current"` (default) or `"wallpaper-only"`).
    fn snapshot_impl(&self, path: &str, scene: &str) -> io::Result<()> {
        let buf = match scene {
            "" | "current" => &self.bufs.current,
            "wallpaper-only" => &self.bufs.wallpaper_only,
            other => {
                return Err(io::Error::other(format!(
                    "snapshot: unknown scene {other:?} (expected `current` or `wallpaper-only`)"
                )));
            }
        };
        let frame = {
            let guard = buf
                .lock()
                .map_err(|_| io::Error::other("snapshot: buffer mutex poisoned"))?;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| io::Error::other("snapshot: no frame composited yet"))?
        };
        write_png(&frame, Path::new(path))
    }
}

#[zbus::interface(name = "org.halmasuit.Debug.Introspect")]
impl Introspect {
    /// Write the most-recently-composited frame to `path` as a PNG.
    /// Errors (no frame yet, unwritable path) surface as D-Bus errors.
    // reason: zbus's `#[interface]` macro deserializes each method
    // argument into an owned value; `&str` is not a valid zbus method
    // parameter type, so the by-value `String` is required here.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "zbus #[interface] method args must be owned (deserialized by value)"
    )]
    fn snapshot(&self, path: String) -> zbus::fdo::Result<()> {
        self.snapshot_impl(&path, "current")
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Like `Snapshot` but selects a slot by name. `scene` is one of:
    ///
    /// - `"current"` — the full live composition (same as `Snapshot`)
    /// - `"wallpaper-only"` — the auxiliary capture of just the
    ///   wallpaper element, populated alongside every audited frame.
    ///   Variant-distinct across the matrix cells even when niri's
    ///   xdg_toplevel covers the wallpaper in the live composition.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "zbus #[interface] method args must be owned (deserialized by value)"
    )]
    fn snapshot_scene(&self, path: String, scene: String) -> zbus::fdo::Result<()> {
        self.snapshot_impl(&path, &scene)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

/// Spawn the D-Bus server thread. Owns `org.halmasuit` on the **system
/// bus** (halmasuit runs as a system service) and serves the
/// `Introspect` object at `/org/halmasuit/Debug/Introspect`. The
/// blocking zbus connection runs its own executor; this thread parks
/// to keep the connection (and thus the served object) alive for the
/// process lifetime.
///
/// Best-effort: a bus that is unreachable or a name that is policy-
/// denied logs a warning and the thread exits — frame_audit's
/// `FrameRendered` stream is unaffected, only `Snapshot()` is.
pub fn serve(bufs: SnapshotHandles) {
    // Build the connection on the CALLER's thread, synchronously,
    // before returning. main() calls this before the privilege drop,
    // so the bus connection authenticates as the pre-drop euid (root
    // in production deploys) deterministically — not racing the
    // setresuid the way a thread-internal connect would. The D-Bus
    // policy then only has to grant name ownership to root.
    let Some(conn) = build_connection(bufs) else {
        return;
    };
    park_with_connection(conn);
}

fn build_connection(bufs: SnapshotHandles) -> Option<zbus::blocking::Connection> {
    match zbus::blocking::connection::Builder::system()
        .and_then(|b| b.name("org.halmasuit"))
        .and_then(|b| b.serve_at("/org/halmasuit/Debug/Introspect", Introspect { bufs }))
        .and_then(zbus::blocking::connection::Builder::build)
    {
        Ok(conn) => {
            tracing::info!("frame_audit D-Bus Snapshot() ready on org.halmasuit.Debug.Introspect");
            Some(conn)
        }
        Err(e) => {
            tracing::warn!(error = %e, "frame_audit D-Bus serve failed; Snapshot() unavailable");
            None
        }
    }
}

fn park_with_connection(conn: zbus::blocking::Connection) {
    // Hand the established connection to a holder thread that parks to
    // keep it (and the served object) alive for the process lifetime.
    // zbus dispatches method calls on its own internal executor.
    std::thread::Builder::new()
        .name("halmasuit-dbus".to_owned())
        .spawn(move || {
            let _conn = conn;
            loop {
                std::thread::park();
            }
        })
        .expect("spawn halmasuit-dbus thread");
}

#[cfg(test)]
mod tests {
    use super::{FrameBuf, Introspect, SnapshotHandles, new_buffer, write_png};

    fn solid(w: usize, h: usize, rgba: [u8; 4]) -> FrameBuf {
        FrameBuf {
            rgba: rgba.iter().copied().cycle().take(w * h * 4).collect(),
            width: w,
            height: h,
        }
    }

    fn empty_handles() -> SnapshotHandles {
        SnapshotHandles {
            current: new_buffer(),
            wallpaper_only: new_buffer(),
        }
    }

    #[test]
    fn write_png_roundtrips_dimensions_and_pixels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("snap.png");
        let frame = solid(7, 5, [0x16, 0xC4, 0x4E, 0xFF]);
        write_png(&frame, &path).expect("write_png");

        let decoder = png::Decoder::new(std::fs::File::open(&path).expect("open"));
        let mut reader = decoder.read_info().expect("png info");
        let mut out = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut out).expect("png frame");
        assert_eq!(info.width, 7);
        assert_eq!(info.height, 5);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        // Every pixel is the green we wrote.
        assert!(
            out[..info.buffer_size()]
                .chunks_exact(4)
                .all(|p| p == [0x16, 0xC4, 0x4E, 0xFF])
        );
    }

    #[test]
    fn write_png_rejects_short_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.png");
        let bad = FrameBuf {
            rgba: vec![0; 4],
            width: 16,
            height: 16,
        };
        let err = write_png(&bad, &path).expect_err("must reject short buffer");
        assert!(err.to_string().contains("< "), "{err}");
    }

    #[test]
    fn snapshot_without_frame_errors() {
        let iface = Introspect {
            bufs: empty_handles(),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("none.png");
        let err = iface
            .snapshot_impl(path.to_str().unwrap(), "current")
            .expect_err("no frame yet must error");
        assert!(err.to_string().contains("no frame composited"), "{err}");
        assert!(!path.exists(), "no file should be written when empty");
    }

    #[test]
    fn snapshot_writes_published_frame() {
        let bufs = empty_handles();
        *bufs.current.lock().unwrap() = Some(solid(4, 4, [10, 20, 30, 255]));
        let iface = Introspect { bufs };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ok.png");
        iface
            .snapshot_impl(path.to_str().unwrap(), "current")
            .expect("snapshot of published frame");
        assert!(path.exists());
    }

    #[test]
    fn snapshot_scene_routes_to_wallpaper_only_slot() {
        let bufs = empty_handles();
        // `current` is empty; `wallpaper_only` has a frame. A scene
        // arg of "wallpaper-only" must read from the latter without
        // erroring on the former.
        *bufs.wallpaper_only.lock().unwrap() = Some(solid(3, 3, [55, 66, 77, 255]));
        let iface = Introspect { bufs };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wp.png");
        iface
            .snapshot_impl(path.to_str().unwrap(), "wallpaper-only")
            .expect("snapshot of wallpaper-only slot");
        assert!(path.exists());
    }

    #[test]
    fn snapshot_scene_empty_string_routes_to_current_slot() {
        // The `match scene` arm at snapshot_impl pairs `""` with
        // `"current"`. Pin that mapping so an accidental split into
        // two separate arms (with the empty-string variant dropped)
        // is caught — busctl callers occasionally pass `""` from
        // shell scripts where the variable expanded to nothing, and
        // the contract is "treat as current", not "reject".
        let bufs = empty_handles();
        *bufs.current.lock().unwrap() = Some(solid(2, 2, [11, 22, 33, 255]));
        let iface = Introspect { bufs };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.png");
        iface
            .snapshot_impl(path.to_str().unwrap(), "")
            .expect("empty scene must route to current");
        assert!(path.exists());
    }

    #[test]
    fn snapshot_scene_unknown_value_errors() {
        let iface = Introspect {
            bufs: empty_handles(),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("z.png");
        let err = iface
            .snapshot_impl(path.to_str().unwrap(), "made-up-scene")
            .expect_err("unknown scene must error");
        assert!(err.to_string().contains("unknown scene"), "{err}");
        assert!(!path.exists());
    }
}
