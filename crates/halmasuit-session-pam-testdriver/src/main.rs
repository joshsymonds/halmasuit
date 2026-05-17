//! Test-only driver for the `run-pam-auth` VM gate (Epic #1 R12).
//!
//! Opens a real `SOCK_SEQPACKET` socketpair, plays the compositor /
//! greeter side on a thread (answering every PAM prompt with the
//! supplied password), and calls the REAL
//! `halmasuit_session::run_pam_auth` against the REAL libpam stack —
//! no mock, no PAM bypass (CLAUDE.md hard rule). Prints the resolved
//! identity for the testScript to assert; exits non-zero on any auth
//! failure; a watchdog guarantees it never hangs CI.
//!
//! `#![forbid(unsafe_code)]`: the only unsafe in this dependency graph
//! is quarantined in `halmasuit-session::pam_ffi`.
#![forbid(unsafe_code)]

use std::time::Duration;
use std::{env, fs, process, thread};

use halmasuit_session::{SeqpacketChannel, run_pam_auth};
use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, Secret};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <service> <username> <password-file>", args[0]);
        process::exit(64);
    }
    let service = args[1].clone();
    let username = args[2].clone();
    let raw = fs::read_to_string(&args[3]).expect("read password file");
    let password = raw.strip_suffix('\n').unwrap_or(&raw).to_owned();

    // Watchdog: a wedged PAM module must never hang the gate. Bounded,
    // distinct exit code so a timeout is unambiguous in the testScript.
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(30));
        eprintln!("ERR timeout: run_pam_auth did not complete in 30s");
        process::exit(2);
    });

    let (broker_fd, peer_fd) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .expect("socketpair");
    let broker = SeqpacketChannel::new(broker_fd);
    let peer = SeqpacketChannel::new(peer_fd);

    // Greeter side: every prompt the broker relays is answered with the
    // password. (ChannelResponder only forwards response-expecting
    // styles; Info/Error never reach the channel.) Loop until the
    // broker end is dropped, which ends the recv.
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

    let result = run_pam_auth(&broker, &service, &username);
    drop(broker); // unblock the greeter thread's pending recv
    let _ = greeter.join();

    match result {
        Ok(id) => {
            // Parseable line for the testScript. id.* is PAM-resolved
            // (Epic R8): uid/gid come from the resolved name's pwent.
            println!("OK user={} uid={} gid={}", id.username, id.uid, id.gid);
            process::exit(0);
        }
        Err(e) => {
            eprintln!("ERR {e}");
            process::exit(1);
        }
    }
}
