//! Lifecycle integration tests for the halmasuit binary.
//!
//! These spawn the actual `halmasuit` binary, read stderr line by line, send
//! POSIX signals, and assert that the lifecycle events documented in the
//! halmasuit-introspect Event schema land on the wire. No mocking — real
//! tracing-subscriber, real calloop, real signalfd.
//!
//! The tracing-subscriber JSON formatter wraps each emit() call in its own
//! envelope (timestamp, level, target, fields). Our inner JSON sits in
//! `fields.json` as a string. These helpers parse both layers.
//!
//! Epic #47 R2.2 contract: SIGTERM emits `Event::Shutdown` and runs the
//! graceful tear-down, but halmasuit KEEPS RUNNING — it does NOT exit.
//! Survival through kernel halt is what carries the wallpaper plane
//! through systemd-shutdown's rootfs→shutdownRamfs pivot. These tests
//! therefore observe the event then SIGKILL to clean up the child.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tempfile::TempDir;

const TIMEOUT_EVENT: Duration = Duration::from_secs(3);

/// Spawn the halmasuit binary with stderr piped, and a background thread
/// shuttling stderr lines into a channel. Returns the child handle plus the
/// receiver end. Killing the child or dropping the receiver tears the
/// background thread down.
/// Spawn the halmasuit binary in an isolated XDG_RUNTIME_DIR (per-test
/// tempdir) so the Wayland socket binding doesn't collide between
/// concurrent tests and the host's runtime directory. Returns the
/// child, a channel of stderr lines, and the tempdir (kept alive for
/// the test's duration; dropped when the test scope ends).
fn spawn() -> (Child, mpsc::Receiver<String>, TempDir) {
    let runtime_dir = tempfile::Builder::new()
        .prefix("halmasuit-lifecycle-")
        .tempdir()
        .expect("create test runtime tempdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_halmasuit"))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        // Lifecycle tests run on the cargo build host, which has no
        // /dev/dri/card0. The production path acquires DRM master
        // before any other init; bypass it here so these tests can
        // exercise the rest of the lifecycle without a real GPU.
        // The VM test exercises the real DRM master path.
        .env("HALMASUIT_SKIP_DRM_MASTER", "1")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn halmasuit binary");

    let stderr = child.stderr.take().expect("piped stderr handle");
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx, runtime_dir)
}

/// Pull the next halmasuit-introspect event line, skipping over any
/// tracing-subscriber envelopes from other crates (smithay emits its
/// own info-level events at the same level on stderr). Filters by
/// `target == "halmasuit::event"`, parses the two-layer JSON, and
/// returns the inner halmasuit-introspect payload.
fn next_event(rx: &mpsc::Receiver<String>) -> serde_json::Value {
    let deadline = Instant::now() + TIMEOUT_EVENT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(remaining) {
            Ok(s) => s,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("no halmasuit::event line within {TIMEOUT_EVENT:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("halmasuit stderr closed before event arrived")
            }
        };
        let envelope: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("envelope parse failed for {line:?}: {e}"));
        // Skip lines that aren't our event-target — smithay, calloop,
        // etc. all emit on the same stderr stream.
        if envelope["target"] != "halmasuit::event" {
            continue;
        }
        let inner = envelope["fields"]["json"].as_str().unwrap_or_else(|| {
            panic!("halmasuit::event envelope missing fields.json string: {envelope}")
        });
        return serde_json::from_str(inner)
            .unwrap_or_else(|e| panic!("inner JSON parse failed for {inner:?}: {e}"));
    }
}

fn send_signal(child: &Child, sig: Signal) {
    let pid = i32::try_from(child.id()).expect("Linux PID fits in i32");
    signal::kill(Pid::from_raw(pid), sig).expect("kill(2) failed");
}

/// Epic #47 R2.2 contract: halmasuit's main loop never exits on its
/// own — survival through systemd-shutdown's kill spree is what
/// preserves the wallpaper plane across the rootfs→shutdownRamfs
/// pivot. Tests that observe a `Shutdown` event must SIGKILL the
/// child to terminate it, never wait on a clean exit.
fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Assert that the child is STILL running a brief moment after a
/// SIGTERM. The R2.2 contract is that SIGTERM emits the Shutdown
/// event and runs the wallpaper-only tear-down but does NOT exit
/// the main loop. A zero exit at this point would mean halmasuit
/// regressed back to the R2.1 contract and the wallpaper would
/// black out during the pivot.
fn assert_still_running(child: &mut Child) {
    // 50ms is enough for graceful_shutdown to have unwound from
    // the signal handler — the calloop dispatch returns
    // immediately after — but well under any plausible exit window
    // if the contract were broken.
    thread::sleep(Duration::from_millis(50));
    match child.try_wait().expect("try_wait failed") {
        None => {} // still running — correct
        Some(status) => panic!(
            "halmasuit exited ({status:?}) after SIGTERM — R2.2 contract \
             requires the process to keep painting wallpaper until \
             external SIGKILL / kernel halt"
        ),
    }
}

#[test]
fn emits_started_init_then_wayland_ready_within_one_second() {
    let (child, rx, runtime_dir) = spawn();

    let started = next_event(&rx);
    assert_eq!(started["event"], "started", "first event: {started}");
    assert!(
        started["pid"].as_u64().is_some(),
        "started event must carry numeric pid: {started}"
    );
    assert!(
        started["version"].as_str().is_some(),
        "started event must carry version string: {started}"
    );

    let init = next_event(&rx);
    assert_eq!(init["event"], "phase_entered", "second event: {init}");
    assert_eq!(
        init["phase"], "init",
        "second event must be Phase::Init: {init}"
    );

    let wayland_ready = next_event(&rx);
    assert_eq!(
        wayland_ready["event"], "phase_entered",
        "third event: {wayland_ready}"
    );
    assert_eq!(
        wayland_ready["phase"], "wayland_ready",
        "third event must be Phase::WaylandReady: {wayland_ready}"
    );

    let greetd_ready = next_event(&rx);
    assert_eq!(
        greetd_ready["event"], "phase_entered",
        "fourth event: {greetd_ready}"
    );
    assert_eq!(
        greetd_ready["phase"], "greetd_ready",
        "fourth event must be Phase::GreetdReady: {greetd_ready}"
    );

    // Both sockets must exist on disk at this point.
    let wayland_socket = runtime_dir.path().join("wayland-0");
    assert!(
        wayland_socket.exists(),
        "wayland-0 socket must exist at {}",
        wayland_socket.display()
    );
    let greetd_socket = runtime_dir.path().join("greetd.sock");
    assert!(
        greetd_socket.exists(),
        "greetd socket must exist at {}",
        greetd_socket.display()
    );

    send_signal(&child, Signal::SIGTERM);
    kill_and_reap(child);
}

#[test]
fn sigterm_emits_shutdown_signal_term_and_keeps_running() {
    let (mut child, rx, _runtime_dir) = spawn();

    // Drain startup events.
    let _ = next_event(&rx); // started
    let _ = next_event(&rx); // phase_entered init
    let _ = next_event(&rx); // phase_entered wayland_ready
    let _ = next_event(&rx); // phase_entered greetd_ready

    send_signal(&child, Signal::SIGTERM);

    let shutdown = next_event(&rx);
    assert_eq!(shutdown["event"], "shutdown", "shutdown event: {shutdown}");
    assert_eq!(
        shutdown["reason"], "signal_term",
        "SIGTERM must map to signal_term: {shutdown}"
    );

    assert_still_running(&mut child);
    kill_and_reap(child);
}

#[test]
fn sigint_emits_shutdown_signal_int_and_keeps_running() {
    let (mut child, rx, _runtime_dir) = spawn();

    let _ = next_event(&rx); // started
    let _ = next_event(&rx); // phase_entered init
    let _ = next_event(&rx); // phase_entered wayland_ready
    let _ = next_event(&rx); // phase_entered greetd_ready

    send_signal(&child, Signal::SIGINT);

    let shutdown = next_event(&rx);
    assert_eq!(shutdown["event"], "shutdown", "shutdown event: {shutdown}");
    assert_eq!(
        shutdown["reason"], "signal_int",
        "SIGINT must map to signal_int: {shutdown}"
    );

    assert_still_running(&mut child);
    kill_and_reap(child);
}

#[test]
fn tracing_target_is_halmasuit_event() {
    // Sanity check: the tracing-subscriber envelope must carry our target so
    // downstream filters (journald, custom Layers) can route on it. Failure
    // here would mean emit() lost the `target:` attribute somewhere.
    let (child, rx, _runtime_dir) = spawn();

    // Scan stderr until we find a halmasuit::event-targeted envelope.
    // Other tracing events (smithay, calloop) share stderr; we want our
    // one specifically.
    let deadline = Instant::now() + TIMEOUT_EVENT;
    let envelope: serde_json::Value = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(remaining) {
            Ok(s) => s,
            Err(e) => {
                let _ = signal::kill(
                    Pid::from_raw(i32::try_from(child.id()).unwrap()),
                    Signal::SIGTERM,
                );
                panic!("no halmasuit::event envelope within {TIMEOUT_EVENT:?}: {e:?}");
            }
        };
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("envelope parse");
        if parsed["target"] == "halmasuit::event" {
            break parsed;
        }
    };
    assert_eq!(
        envelope["target"], "halmasuit::event",
        "tracing target lost: {envelope}"
    );

    send_signal(&child, Signal::SIGTERM);
    kill_and_reap(child);
}
