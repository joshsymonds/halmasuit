//! Test-only driver for the `run-pam-auth` VM gate (Epic #1 R12 / R4).
//!
//! Two modes, both exercising REAL libpam (no mock, no PAM bypass):
//!
//! - default `<service> <username> <password-file>` — calls the REAL
//!   `run_pam_auth` IN-PROCESS, a sibling thread playing the greeter.
//! - `… --via-fork` — calls `spawn_auth_worker` so PAM runs in the
//!   ephemeral SIGKILL-able PRIVILEGED fork (Epic R4); this process is
//!   the broker PARENT, relaying the conversation over the returned
//!   channel and reading the terminal `WorkerOutcome`, then reaping
//!   the child.
//!
//! Prints `OK user=… uid=… gid=…` (PAM-resolved identity, Epic R8) on
//! success; non-zero on any failure; a 30s watchdog guarantees it
//! never hangs CI.
//!
//! `#![forbid(unsafe_code)]`: the only unsafe in this dependency graph
//! is quarantined in `halmasuit-session::{pam_ffi,worker}`.
#![forbid(unsafe_code)]

use std::time::Duration;
use std::{env, fs, process, thread};

use halmasuit_session::{
    AuthSlot, ParentMessage, SeqpacketChannel, SlotError, WorkerOutcome, run_pam_auth,
    spawn_auth_worker,
};
use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, Secret};
use nix::sys::signal::Signal;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use nix::sys::wait::WaitStatus;

fn spawn_watchdog() {
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(30));
        eprintln!("ERR timeout: auth did not complete in 30s");
        process::exit(2);
    });
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let via_fork = args.iter().any(|a| a == "--via-fork");
    let evict_demo = args.iter().any(|a| a == "--evict-demo");
    let positional: Vec<&String> = args[1..].iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() != 3 {
        eprintln!(
            "usage: {} <service> <username> <password-file> [--via-fork|--evict-demo]",
            args[0]
        );
        process::exit(64);
    }
    let service = positional[0].clone();
    let username = positional[1].clone();
    let raw = fs::read_to_string(positional[2]).expect("read password file");
    let password = raw.strip_suffix('\n').unwrap_or(&raw).to_owned();

    if evict_demo {
        run_evict_demo(&service, &username, &password);
    } else if via_fork {
        run_via_fork(&service, &username, &password);
    } else {
        run_in_process(&service, &username, password);
    }
}

/// Epic R5 path: a real in-flight `spawn_auth_worker` (blocked in real
/// `pam_authenticate`) is evicted via `AuthSlot`. Proves: a non-greeter
/// peer cannot evict (in-flight untouched); an authorized greeter
/// create SIGKILLs+reaps the real in-flight worker (R4, no SIGTERM);
/// a fresh real auth then succeeds; the worker is reaped (no orphan).
fn run_evict_demo(service: &str, username: &str, password: &str) {
    const GREETER: u32 = 1000; // the VM test user's uid
    const NON_GREETER: u32 = 9999;

    let mut slot = AuthSlot::with_defaults(GREETER);

    // In-flight #1: a REAL worker. Both spawn_auth_worker forks happen
    // BEFORE the watchdog thread (single-threaded fork discipline).
    slot.create(GREETER, || spawn_auth_worker(service, username))
        .expect("authorized create #1");
    let pid1 = slot.current().expect("inflight #1").pid;

    // Drive #1 just far enough to be genuinely blocked in real
    // pam_authenticate: read its first ConvPrompt and DO NOT answer.
    match slot.current().unwrap().channel().recv::<ParentMessage>() {
        Ok(ParentMessage::Conv(BrokerToCompositor::ConvPrompt { .. })) => {}
        other => {
            eprintln!("ERR worker #1 did not reach a conv prompt: {other:?}");
            let _ = slot.reap_current();
            process::exit(1);
        }
    }

    // A non-greeter peer must NOT be able to evict the real in-flight
    // worker. spawn_auth_worker is NOT called (gate rejects first).
    match slot.create(NON_GREETER, || spawn_auth_worker(service, username)) {
        Err(SlotError::Unauthorized) => {}
        other => {
            eprintln!("ERR non-greeter evict not refused: {other:?}");
            let _ = slot.reap_current();
            process::exit(1);
        }
    }
    let unchanged = slot.current().unwrap().pid == pid1;
    println!("EVICT_DEMO unauthorized_refused inflight_untouched={unchanged}");

    // Authorized greeter create EVICTS the real in-flight worker:
    // SIGKILL (R4 — no SIGTERM) + reap, then a fresh worker.
    let evicted = slot
        .create(GREETER, || spawn_auth_worker(service, username))
        .expect("authorized evict create #2");
    let killed = matches!(evicted, Some(WaitStatus::Signaled(_, Signal::SIGKILL, _)));
    let pid2 = slot.current().expect("inflight #2").pid;
    println!(
        "EVICT_DEMO evicted_sigkill={killed} pid_changed={} evicted={evicted:?}",
        pid2 != pid1
    );

    spawn_watchdog();

    // Drive the fresh worker #2 to a real success.
    let outcome = {
        let chan = slot.current().unwrap().channel();
        loop {
            match chan.recv::<ParentMessage>() {
                Ok(ParentMessage::Conv(BrokerToCompositor::ConvPrompt { .. })) => {
                    if chan
                        .send(&CompositorToBroker::ConvResponse {
                            response: Secret::new(password.to_owned()),
                        })
                        .is_err()
                    {
                        eprintln!("ERR channel closed answering #2");
                        break None;
                    }
                }
                Ok(ParentMessage::Conv(other)) => {
                    eprintln!("ERR unexpected conv from #2: {other:?}");
                    break None;
                }
                Ok(ParentMessage::Outcome(o)) => break Some(o),
                Err(_) => {
                    eprintln!("ERR #2 channel closed before outcome");
                    break None;
                }
            }
        }
    };

    let ok = matches!(
        &outcome,
        Some(WorkerOutcome::Success { username, uid, gid })
            if username == "test" && *uid == 1000 && *gid == 1000
    );
    if let Some(WorkerOutcome::Success { username, uid, gid }) = &outcome {
        println!("OK user={username} uid={uid} gid={gid}");
    } else {
        eprintln!("ERR fresh auth did not succeed: {outcome:?}");
    }

    // Reap the fresh worker — no orphan must survive.
    let reaped = slot.reap_current();
    let reaped_ok = matches!(reaped, Some(Ok(WaitStatus::Exited(_, 0))));
    println!("EVICT_DEMO fresh_reaped_ok={reaped_ok} reaped={reaped:?}");

    if killed && unchanged && ok && reaped_ok {
        process::exit(0);
    }
    process::exit(1);
}

/// Epic R4 path: PAM runs in the disposable privileged fork; this
/// process is the broker parent relaying the conversation.
fn run_via_fork(service: &str, username: &str, password: &str) {
    // spawn_auth_worker forks. Do it BEFORE starting the watchdog
    // thread so the fork happens single-threaded (a forked child that
    // then runs libpam must not inherit a multithreaded address space
    // mid-malloc — the OpenSSH privsep discipline).
    let (handle, chan) = spawn_auth_worker(service, username).expect("spawn_auth_worker");
    spawn_watchdog();

    // Relay the one channel: conv prompts get the password; the
    // terminal WorkerOutcome ends it. ParentMessage's disjoint tag
    // namespaces make this decode unambiguous (see worker.rs).
    let exit = loop {
        match chan.recv::<ParentMessage>() {
            Ok(ParentMessage::Conv(BrokerToCompositor::ConvPrompt { .. })) => {
                if chan
                    .send(&CompositorToBroker::ConvResponse {
                        response: Secret::new(password.to_owned()),
                    })
                    .is_err()
                {
                    eprintln!("ERR channel closed while answering prompt");
                    break 1;
                }
            }
            Ok(ParentMessage::Conv(other)) => {
                eprintln!("ERR unexpected conv frame from worker: {other:?}");
                let _ = handle.kill();
                break 1;
            }
            Ok(ParentMessage::Outcome(WorkerOutcome::Success { username, uid, gid })) => {
                println!("OK user={username} uid={uid} gid={gid}");
                break 0;
            }
            Ok(ParentMessage::Outcome(WorkerOutcome::Failure { reason })) => {
                eprintln!("ERR {reason}");
                break 1;
            }
            Ok(ParentMessage::Outcome(
                o @ (WorkerOutcome::AuthOk { .. }
                | WorkerOutcome::SessionOpened { .. }
                | WorkerOutcome::SessionEnded { .. }),
            )) => {
                // spawn_auth_worker is auth-only; a session-phase
                // outcome here is a protocol violation, not success.
                eprintln!("ERR auth-only worker emitted session-phase outcome: {o:?}");
                let _ = handle.kill();
                break 1;
            }
            Err(_) => {
                eprintln!("ERR worker channel closed before an outcome");
                break 1;
            }
        }
    };

    // Reap the ephemeral child — the gate asserts no orphan remains.
    match handle.wait() {
        Ok(status) => eprintln!("worker reaped: {status:?}"),
        Err(e) => eprintln!("ERR reaping worker: {e}"),
    }
    process::exit(exit);
}

/// In-process path (Epic R12 gate from task #8): real `run_pam_auth`
/// here, a sibling thread playing the greeter. No fork.
fn run_in_process(service: &str, username: &str, password: String) {
    spawn_watchdog();

    let (broker_fd, peer_fd) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .expect("socketpair");
    let broker = SeqpacketChannel::new(broker_fd);
    let peer = SeqpacketChannel::new(peer_fd);

    let greeter = thread::spawn(move || {
        while let Ok(BrokerToCompositor::ConvPrompt { .. }) = peer.recv::<BrokerToCompositor>() {
            if peer
                .send(&CompositorToBroker::ConvResponse {
                    response: Secret::new(password.clone()),
                })
                .is_err()
            {
                break;
            }
        }
    });

    let result = run_pam_auth(&broker, service, username);
    drop(broker);
    let _ = greeter.join();

    match result {
        Ok(id) => {
            println!("OK user={} uid={} gid={}", id.username, id.uid, id.gid);
            process::exit(0);
        }
        Err(e) => {
            eprintln!("ERR {e}");
            process::exit(1);
        }
    }
}
