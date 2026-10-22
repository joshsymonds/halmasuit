//! Hotplug for the libinput context halmasuit opens directly.
//!
//! An inotify watch on `/dev/input` and `/run/udev/data` drives a
//! non-blocking calloop source. Each wake rescans `/dev/input` and adds
//! every `event*` character device that libinput does not already hold
//! and whose udev database entry exists. The gate matters because
//! `path_add_device` sleeps on the calling thread until udev has
//! initialized the node (up to 200 x 10 ms), and rejects nodes that
//! lack the `ID_INPUT` tag. Admitting only nodes that have an entry
//! keeps that wait off the calloop thread.
//!
//! "Held" is decided by device number (`st_rdev`) over the fds libinput
//! currently holds. `DirectInputInterface` maintains the registry: it
//! counts an open on `open_restricted` and releases it on
//! `close_restricted`, which libinput calls when it drops a device.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;
use std::rc::Rc;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use nix::sys::stat::{major, minor};
use smithay::reexports::input::Libinput;

const DEV_INPUT: &str = "/dev/input";
const UDEV_DATA: &str = "/run/udev/data";

/// Events on `/dev/input` that can change which nodes are candidates:
/// creation, removal, a metadata change (udevd sets the group before it
/// writes the database entry), or a rename into place.
const DEV_INPUT_MASK: AddWatchFlags = AddWatchFlags::IN_CREATE
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_MOVED_TO);

/// Events on the udev database that make a node admissible: udevd
/// writes each entry by rename, so `IN_MOVED_TO` is the arrival signal.
const UDEV_DATA_MASK: AddWatchFlags = AddWatchFlags::IN_CREATE.union(AddWatchFlags::IN_MOVED_TO);

/// Open fds libinput holds, counted per device number (`st_rdev`).
#[derive(Debug, Default)]
pub struct HeldDevices {
    counts: HashMap<u64, usize>,
}

impl HeldDevices {
    pub fn open(&mut self, rdev: u64) {
        *self.counts.entry(rdev).or_default() += 1;
    }

    pub fn close(&mut self, rdev: u64) {
        if let Some(count) = self.counts.get_mut(&rdev) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&rdev);
            }
        }
    }

    pub fn contains(&self, rdev: u64) -> bool {
        self.counts.contains_key(&rdev)
    }
}

/// Name of the udev database entry for a device number:
/// `c<major>:<minor>` under `/run/udev/data`.
pub fn udev_db_entry_name(rdev: u64) -> String {
    format!("c{}:{}", major(rdev), minor(rdev))
}

/// Whether a `/dev/input` node should be handed to libinput now.
pub fn admits(
    name: &str,
    is_char_device: bool,
    rdev: u64,
    held: &HeldDevices,
    udev_dir: &Path,
) -> bool {
    name.starts_with("event")
        && is_char_device
        && !held.contains(rdev)
        && udev_dir.join(udev_db_entry_name(rdev)).exists()
}

/// Watch `/dev/input` and the udev database, rescan once, then rescan on
/// every batch of events. Returns the calloop registration token.
pub fn start<D: 'static>(
    loop_handle: &LoopHandle<'static, D>,
    mut context: Libinput,
    held: Rc<RefCell<HeldDevices>>,
) -> io::Result<RegistrationToken> {
    let inotify =
        Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).map_err(io::Error::from)?;
    inotify
        .add_watch(DEV_INPUT, DEV_INPUT_MASK)
        .map_err(io::Error::from)?;
    inotify
        .add_watch(UDEV_DATA, UDEV_DATA_MASK)
        .map_err(io::Error::from)?;
    rescan(&mut context, &held);
    loop_handle
        .insert_source(
            Generic::new(inotify, Interest::READ, Mode::Level),
            move |_, inotify, _: &mut D| match drain(inotify) {
                Ok(true) => {
                    rescan(&mut context, &held);
                    Ok(PostAction::Continue)
                }
                Ok(false) => Ok(PostAction::Continue),
                Err(e) => {
                    tracing::error!(error = %e, "input hotplug: inotify read failed; hotplug stopped");
                    Ok(PostAction::Remove)
                }
            },
        )
        .map_err(|e| io::Error::other(format!("insert input hotplug watch: {e}")))
}

/// Read every queued event without blocking. Returns whether any arrived.
fn drain(inotify: &Inotify) -> Result<bool, Errno> {
    let mut any = false;
    loop {
        match inotify.read_events() {
            Ok(events) => any |= !events.is_empty(),
            Err(Errno::EAGAIN) => return Ok(any),
            Err(e) => return Err(e),
        }
    }
}

/// Add every admissible `/dev/input/event*` node to `context`.
fn rescan(context: &mut Libinput, held: &RefCell<HeldDevices>) {
    let entries = match std::fs::read_dir(DEV_INPUT) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "input hotplug: read_dir /dev/input failed");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(path_str) = path.to_str() else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        // The borrow ends with this statement: `path_add_device` re-enters
        // `DirectInputInterface::open_restricted`, which takes `borrow_mut`.
        let admitted = admits(
            &name,
            metadata.file_type().is_char_device(),
            metadata.rdev(),
            &held.borrow(),
            Path::new(UDEV_DATA),
        );
        if !admitted {
            continue;
        }
        if context.path_add_device(path_str).is_some() {
            tracing::info!(path = path_str, "input hotplug: added device");
        } else {
            tracing::debug!(path = path_str, "input hotplug: libinput refused device");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::stat::makedev;

    fn udev_dir_with_c13_64() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("c13:64"), b"").expect("write entry");
        dir
    }

    #[test]
    fn udev_entry_name_is_char_major_minor() {
        assert_eq!(udev_db_entry_name(makedev(13, 64)), "c13:64");
        assert_eq!(udev_db_entry_name(makedev(259, 1)), "c259:1");
    }

    #[test]
    fn admits_event_char_device_with_udev_entry_and_not_held() {
        let rdev = makedev(13, 64);
        let udev = udev_dir_with_c13_64();
        assert!(admits(
            "event3",
            true,
            rdev,
            &HeldDevices::default(),
            udev.path()
        ));
    }

    #[test]
    fn refuses_non_event_name() {
        let rdev = makedev(13, 64);
        let udev = udev_dir_with_c13_64();
        assert!(!admits(
            "mouse0",
            true,
            rdev,
            &HeldDevices::default(),
            udev.path()
        ));
    }

    #[test]
    fn refuses_non_char_file() {
        let rdev = makedev(13, 64);
        let udev = udev_dir_with_c13_64();
        assert!(!admits(
            "event3",
            false,
            rdev,
            &HeldDevices::default(),
            udev.path()
        ));
    }

    #[test]
    fn refuses_held_device_number() {
        let rdev = makedev(13, 64);
        let udev = udev_dir_with_c13_64();
        let mut held = HeldDevices::default();
        held.open(rdev);
        assert!(!admits("event3", true, rdev, &held, udev.path()));
    }

    #[test]
    fn refuses_missing_udev_entry() {
        let rdev = makedev(13, 64);
        let udev = tempfile::tempdir().expect("tempdir");
        assert!(!admits(
            "event3",
            true,
            rdev,
            &HeldDevices::default(),
            udev.path()
        ));
    }

    #[test]
    fn two_opens_then_one_close_is_still_held() {
        let rdev = makedev(13, 64);
        let mut held = HeldDevices::default();
        held.open(rdev);
        held.open(rdev);
        held.close(rdev);
        assert!(held.contains(rdev));
    }

    #[test]
    fn second_close_releases_device_number() {
        let rdev = makedev(13, 64);
        let mut held = HeldDevices::default();
        held.open(rdev);
        held.open(rdev);
        held.close(rdev);
        held.close(rdev);
        assert!(!held.contains(rdev));
    }
}
