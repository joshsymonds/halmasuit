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

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

const TIMEOUT_EVENT: Duration = Duration::from_secs(3);
const TIMEOUT_EXIT: Duration = Duration::from_secs(5);
const EXIT_POLL: Duration = Duration::from_millis(50);

/// Spawn the halmasuit binary with stderr piped, and a background thread
/// shuttling stderr lines into a channel. Returns the child handle plus the
/// receiver end. Killing the child or dropping the receiver tears the
/// background thread down.
fn spawn() -> (Child, mpsc::Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_halmasuit"))
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
    (child, rx)
}

/// Pull the next stderr line, parse the tracing-subscriber envelope, and
/// return the inner halmasuit-introspect JSON payload.
fn next_event(rx: &mpsc::Receiver<String>) -> serde_json::Value {
    let line = match rx.recv_timeout(TIMEOUT_EVENT) {
        Ok(s) => s,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("no event line within {TIMEOUT_EVENT:?}")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("halmasuit stderr closed before any event arrived")
        }
    };
    let envelope: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("envelope parse failed for {line:?}: {e}"));
    let inner = envelope["fields"]["json"].as_str().unwrap_or_else(|| {
        panic!("expected envelope.fields.json to be a string, envelope was: {envelope}")
    });
    serde_json::from_str(inner)
        .unwrap_or_else(|e| panic!("inner JSON parse failed for {inner:?}: {e}"))
}

fn send_signal(child: &Child, sig: Signal) {
    let pid = i32::try_from(child.id()).expect("Linux PID fits in i32");
    signal::kill(Pid::from_raw(pid), sig).expect("kill(2) failed");
}

fn wait_for_exit(mut child: Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT_EXIT;
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("halmasuit did not exit within {TIMEOUT_EXIT:?}");
            }
            None => thread::sleep(EXIT_POLL),
        }
    }
}

#[test]
fn emits_started_then_phase_entered_within_one_second() {
    let (child, rx) = spawn();

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

    let phase = next_event(&rx);
    assert_eq!(phase["event"], "phase_entered", "second event: {phase}");
    assert_eq!(
        phase["phase"], "init",
        "second event must be the init phase: {phase}"
    );

    // Clean up; the test doesn't assert on shutdown here, just that startup
    // events fire in order.
    send_signal(&child, Signal::SIGTERM);
    let _ = wait_for_exit(child);
}

#[test]
fn sigterm_emits_shutdown_signal_term_and_exits_zero() {
    let (child, rx) = spawn();

    // Drain startup events.
    let _ = next_event(&rx); // started
    let _ = next_event(&rx); // phase_entered

    send_signal(&child, Signal::SIGTERM);

    let shutdown = next_event(&rx);
    assert_eq!(shutdown["event"], "shutdown", "shutdown event: {shutdown}");
    assert_eq!(
        shutdown["reason"], "signal_term",
        "SIGTERM must map to signal_term: {shutdown}"
    );

    let status = wait_for_exit(child);
    assert!(status.success(), "expected clean exit, got {status:?}");
}

#[test]
fn sigint_emits_shutdown_signal_int_and_exits_zero() {
    let (child, rx) = spawn();

    let _ = next_event(&rx); // started
    let _ = next_event(&rx); // phase_entered

    send_signal(&child, Signal::SIGINT);

    let shutdown = next_event(&rx);
    assert_eq!(shutdown["event"], "shutdown", "shutdown event: {shutdown}");
    assert_eq!(
        shutdown["reason"], "signal_int",
        "SIGINT must map to signal_int: {shutdown}"
    );

    let status = wait_for_exit(child);
    assert!(status.success(), "expected clean exit, got {status:?}");
}

#[test]
fn tracing_target_is_halmasuit_event() {
    // Sanity check: the tracing-subscriber envelope must carry our target so
    // downstream filters (journald, custom Layers) can route on it. Failure
    // here would mean emit() lost the `target:` attribute somewhere.
    let (child, rx) = spawn();

    let line = match rx.recv_timeout(TIMEOUT_EVENT) {
        Ok(s) => s,
        Err(e) => {
            let _ = signal::kill(
                Pid::from_raw(i32::try_from(child.id()).unwrap()),
                Signal::SIGTERM,
            );
            panic!("no envelope line within {TIMEOUT_EVENT:?}: {e:?}");
        }
    };
    let envelope: serde_json::Value = serde_json::from_str(&line).expect("envelope parse");
    assert_eq!(
        envelope["target"], "halmasuit::event",
        "tracing target lost: {envelope}"
    );

    send_signal(&child, Signal::SIGTERM);
    let _ = wait_for_exit(child);
}
