// halmasuit/src/render_node.rs — `HALMASUIT_RENDER_NODE`: the render node
// that names the `zwp_linux_dmabuf_v1` default-feedback main device.
//
// Unset or empty means no render node; the single-device path runs as it
// always has. Otherwise the value is a literal `/dev/dri/renderD<N>` or a
// PCI BDF `pci:DDDD:BB:DD.F` that resolves through sysfs to the render node
// of that device. Resolution retries under the same deadline as the DRM
// device, because udev creates the node asynchronously.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::drm::{BdfParseError, PciBdf};

/// Where the configured render node comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderNodeSpec {
    /// A literal `/dev/dri/renderD<N>` path.
    Path(PathBuf),
    /// The render node of the PCI device at this BDF.
    Pci(PciBdf),
}

/// Errors from [`RenderNodeSpec::from_env_value`].
///
/// The raw input string is deliberately NOT included in the messages, for
/// the same reason as `drm::DrmDeviceSpecParseError`.
#[derive(Debug, Error)]
pub enum RenderNodeSpecParseError {
    /// The value is neither a `/dev/dri/renderD<N>` path nor a `pci:` value.
    #[error("HALMASUIT_RENDER_NODE must be /dev/dri/renderD<N> or pci:DDDD:BB:DD.F")]
    BadShape,
    /// The `pci:`-prefixed value failed BDF validation.
    #[error("HALMASUIT_RENDER_NODE pci: value did not parse as a BDF")]
    BadBdf(#[from] BdfParseError),
}

impl RenderNodeSpec {
    /// Parse a `HALMASUIT_RENDER_NODE` value. Empty input means no render
    /// node and yields `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`RenderNodeSpecParseError::BadBdf`] for a `pci:` value whose
    /// BDF does not parse, and [`RenderNodeSpecParseError::BadShape`] for a
    /// path that is not `/dev/dri/renderD<N>`.
    pub fn from_env_value(s: &str) -> Result<Option<Self>, RenderNodeSpecParseError> {
        if s.is_empty() {
            Ok(None)
        } else if let Some(rest) = s.strip_prefix("pci:") {
            Ok(Some(Self::Pci(PciBdf::parse(rest)?)))
        } else if is_render_node_path(s) {
            Ok(Some(Self::Path(PathBuf::from(s))))
        } else {
            Err(RenderNodeSpecParseError::BadShape)
        }
    }
}

/// Reject anything that isn't structurally `/dev/dri/renderD<N>`.
fn is_render_node_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("/dev/dri/renderD") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Resolve `spec` to a `/dev/dri/renderD<N>` path, retrying until `deadline`
/// while the node is not yet present.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] if no node satisfies `spec` before the
/// deadline; propagates any other I/O error immediately.
pub fn resolve(spec: &RenderNodeSpec, deadline: Duration) -> io::Result<PathBuf> {
    resolve_in(spec, Path::new(crate::drm::SYS_CLASS_DRM), deadline)
}

/// [`resolve`] with the sysfs class directory injected, so tests can point
/// the `pci:` walk at a temporary tree.
fn resolve_in(
    spec: &RenderNodeSpec,
    sys_class_drm: &Path,
    deadline: Duration,
) -> io::Result<PathBuf> {
    crate::drm::poll_until_present("render node", deadline, || match spec {
        RenderNodeSpec::Path(p) => crate::drm::probe_device_path(p),
        RenderNodeSpec::Pci(bdf) => crate::drm::find_dri_node_by_bdf(sys_class_drm, "renderD", bdf),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Instant;

    fn parse(s: &str) -> Result<Option<RenderNodeSpec>, RenderNodeSpecParseError> {
        RenderNodeSpec::from_env_value(s)
    }

    #[test]
    fn empty_value_means_no_render_node() {
        assert!(matches!(parse(""), Ok(None)));
    }

    #[test]
    fn render_path_parses_to_path_spec() {
        match parse("/dev/dri/renderD128").unwrap() {
            Some(RenderNodeSpec::Path(p)) => assert_eq!(p, PathBuf::from("/dev/dri/renderD128")),
            other => panic!("expected Path spec, got {other:?}"),
        }
    }

    #[test]
    fn pci_value_parses_to_pci_spec() {
        match parse("pci:0000:01:00.0").unwrap() {
            Some(RenderNodeSpec::Pci(bdf)) => assert_eq!(bdf.as_str(), "0000:01:00.0"),
            other => panic!("expected Pci spec, got {other:?}"),
        }
    }

    #[test]
    fn non_render_paths_are_rejected() {
        for bad in [
            "/dev/dri/card0",
            "/dev/dri/renderD",
            "/dev/dri/renderD12x",
            "/dev/dri/renderD128/../card0",
            "/dev/dri/renderD-1",
            "/etc/shadow",
            "renderD128",
        ] {
            assert!(
                matches!(parse(bad), Err(RenderNodeSpecParseError::BadShape)),
                "path {bad:?} should be rejected as BadShape"
            );
        }
    }

    #[test]
    fn malformed_pci_bdfs_are_rejected() {
        for bad in [
            "pci:",
            "pci:bogus",
            "pci:00:01:00.0",
            "pci:0000:01:00",
            "pci:0000:01:00.8",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(matches!(
            parse("pci:0000:01:00.8"),
            Err(RenderNodeSpecParseError::BadBdf(BdfParseError::BadFunction))
        ));
    }

    #[test]
    fn parse_errors_do_not_echo_raw_input() {
        let shape = parse("/dev/dri/card-ATTACKER-SECRET")
            .unwrap_err()
            .to_string();
        assert!(
            !shape.contains("ATTACKER"),
            "shape error echoed input: {shape:?}"
        );
        let bdf = parse("pci:ATTACKER-CONTROLLED").unwrap_err().to_string();
        assert!(!bdf.contains("ATTACKER"), "bdf error echoed input: {bdf:?}");
    }

    /// Build a fake `/sys/class/drm` with `device` symlinks whose targets end
    /// in the given PCI BDF, the same shape the kernel emits.
    fn sysfs_with(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        for (node, bdf) in entries {
            let dir = root.path().join(node);
            std::fs::create_dir(&dir).expect("create node dir");
            symlink(
                format!("../../../devices/pci0000:00/{bdf}"),
                dir.join("device"),
            )
            .expect("create device symlink");
        }
        root
    }

    fn pci(bdf: &str) -> RenderNodeSpec {
        RenderNodeSpec::Pci(PciBdf::parse(bdf).expect("valid BDF"))
    }

    #[test]
    fn pci_resolves_to_the_render_node_of_that_device() {
        let root = sysfs_with(&[
            ("card0", "0000:00:02.0"),
            ("renderD128", "0000:00:02.0"),
            ("card1", "0000:01:00.0"),
            ("renderD129", "0000:01:00.0"),
        ]);
        let got = resolve_in(
            &pci("0000:01:00.0"),
            root.path(),
            Duration::from_millis(200),
        )
        .expect("render node should resolve");
        assert_eq!(got, PathBuf::from("/dev/dri/renderD129"));
    }

    #[test]
    fn pci_resolution_ignores_card_entries() {
        // Only a cardN entry carries this BDF; the render resolver must not
        // return it, and with no renderD entry the lookup is not found.
        let root = sysfs_with(&[("card0", "0000:00:02.0")]);
        let err = resolve_in(
            &pci("0000:00:02.0"),
            root.path(),
            Duration::from_millis(200),
        )
        .expect_err("a cardN entry must not satisfy a render node lookup");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn pci_resolution_reports_not_found_for_no_match() {
        let root = sysfs_with(&[("renderD128", "0000:00:02.0")]);
        let deadline = Duration::from_millis(300);
        let start = Instant::now();
        let err = resolve_in(&pci("0000:09:00.0"), root.path(), deadline)
            .expect_err("no matching renderD entry");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(start.elapsed() >= deadline, "gave up before the deadline");
    }

    #[test]
    fn path_mode_returns_an_existing_node_without_sleeping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir.path().join("renderD128");
        std::fs::write(&node, b"").expect("create node stand-in");
        let start = Instant::now();
        let got = resolve_in(
            &RenderNodeSpec::Path(node.clone()),
            dir.path(),
            Duration::from_millis(500),
        )
        .expect("existing node should resolve");
        assert_eq!(got, node);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "resolver slept although the node existed"
        );
    }

    #[test]
    fn path_mode_times_out_on_a_missing_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deadline = Duration::from_millis(300);
        let start = Instant::now();
        let err = resolve_in(
            &RenderNodeSpec::Path(dir.path().join("renderD128")),
            dir.path(),
            deadline,
        )
        .expect_err("missing node must time out");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(start.elapsed() >= deadline, "gave up before the deadline");
    }

    #[test]
    fn path_mode_rejects_a_symlink_to_a_real_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real");
        std::fs::write(&target, b"").expect("create target");
        let link = dir.path().join("renderD128");
        symlink(&target, &link).expect("create symlink");
        let err = resolve_in(
            &RenderNodeSpec::Path(link),
            dir.path(),
            Duration::from_millis(200),
        )
        .expect_err("a symlink must not satisfy the path probe");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
