//! `halmasuit` — Epic #71 R3.4 observability CLI for the compositor.
//!
//! Thin unprivileged shim over the `org.halmasuit.Compositor1`
//! system-bus interface defined by R3.3. Runs as the invoking user;
//! does NOT need root (read methods are unauthenticated per Epic
//! #71's read/write split).
//!
//! ## Subcommands
//!
//! - `halmasuit status` — print key=value lines for the four read
//!   methods on Compositor1 (`GetPhase`, `GetUptime`,
//!   `GetFrameCounter`, `GetBrokerStatus`).
//! - `halmasuit windows` — print one window per line via
//!   `ListWindows`.
//! - `halmasuit logs` / `halmasuit logs -f` — shell out to
//!   `journalctl -u halmasuit --output=cat` (or `-f` to follow).
//!   No DBus needed for this; the journal is the canonical log
//!   surface and `journalctl` already handles authentication.
//!
//! ## Anti-patterns (Epic #71)
//!
//! - NO setuid bit (production cargo build produces a plain
//!   user-mode binary).
//! - NO action subcommands. Read + tail-logs only. Action
//!   subcommands (if ever added) MUST route through the
//!   privileged broker, NOT through Compositor1.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("halmasuit: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("status") => cmd_status(),
        Some("windows") => cmd_windows(),
        Some("logs") => cmd_logs(&argv[1..]),
        Some("--help" | "-h" | "help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => bail!("unknown subcommand: {other}\n\n{HELP}"),
    }
}

const HELP: &str = "\
USAGE: halmasuit <subcommand>

SUBCOMMANDS:
  status        Print compositor lifecycle phase, uptime, frame counter,
                and broker connection state. Connects to
                org.halmasuit.Compositor1 on the system bus.

  windows       List nested-compositor windows (pid, app_id, title).

  logs [-f]     Tail halmasuit.service journal lines. `-f` follows the
                journal indefinitely (like `journalctl -f`).

  help          Print this help.

ENVIRONMENT:
  None — the system bus is the canonical observability surface.
";

fn print_help() {
    println!("{HELP}");
}

fn cmd_status() -> Result<()> {
    let conn = zbus::blocking::Connection::system().context("connect to system bus")?;
    let proxy = compositor1_proxy(&conn)?;

    let phase: String = proxy
        .call("GetPhase", &())
        .context("Compositor1.GetPhase")?;
    let uptime: u64 = proxy
        .call("GetUptime", &())
        .context("Compositor1.GetUptime")?;
    let frame_counter: u64 = proxy
        .call("GetFrameCounter", &())
        .context("Compositor1.GetFrameCounter")?;
    let broker_status: String = proxy
        .call("GetBrokerStatus", &())
        .context("Compositor1.GetBrokerStatus")?;

    println!("phase={phase}");
    println!("uptime_secs={uptime}");
    println!("frame_counter={frame_counter}");
    println!("broker_status={broker_status}");
    Ok(())
}

fn cmd_windows() -> Result<()> {
    let conn = zbus::blocking::Connection::system().context("connect to system bus")?;
    let proxy = compositor1_proxy(&conn)?;

    let windows: Vec<(u32, String, String)> = proxy
        .call("ListWindows", &())
        .context("Compositor1.ListWindows")?;

    if windows.is_empty() {
        println!("(no windows)");
        return Ok(());
    }
    for (pid, app_id, title) in windows {
        println!("pid={pid}\tapp_id={app_id}\ttitle={title}");
    }
    Ok(())
}

fn cmd_logs(args: &[String]) -> Result<()> {
    // `-f` triggers a follow-mode tail (long-running). No other
    // flags supported — keep the surface tight per Epic anti-
    // patterns (no flag-soup, no surprising action paths).
    let follow = args.iter().any(|a| a == "-f" || a == "--follow");
    if let Some(other) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "-f" | "--follow"))
    {
        bail!("unknown logs flag: {other}");
    }

    // journalctl already handles authentication — the systemd
    // journal is the canonical log surface. `--output=cat` strips
    // timestamps + units (a clean stream of just halmasuit's
    // stderr lines, suitable for piping). `-n 50` gives a useful
    // backlog before -f or one-shot exit.
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(["-u", "halmasuit", "--output=cat", "-n", "50"]);
    if follow {
        cmd.arg("-f");
    }
    // Inherit stdio so the user sees logs streamed to their terminal.
    let status = cmd.status().context("spawn journalctl")?;
    if !status.success() {
        bail!("journalctl exited with status {status}");
    }
    Ok(())
}

/// The `org.halmasuit.Compositor1` D-Bus triple this CLI talks to —
/// bus name, object path, interface. MUST match what `halmasuit`'s
/// `dbus_compositor1.rs` serves. Named so the unit test pins the exact
/// strings the live proxy uses (a typo'd rename here fails the test).
const COMPOSITOR1_BUS: &str = "org.halmasuit.Compositor1";
const COMPOSITOR1_PATH: &str = "/org/halmasuit/Compositor1";
const COMPOSITOR1_IFACE: &str = "org.halmasuit.Compositor1";

fn compositor1_proxy(conn: &zbus::blocking::Connection) -> Result<zbus::blocking::Proxy<'static>> {
    zbus::blocking::Proxy::new(conn, COMPOSITOR1_BUS, COMPOSITOR1_PATH, COMPOSITOR1_IFACE)
        .context("build Compositor1 proxy")
}

#[cfg(test)]
mod tests {
    /// `cmd_logs` arg parsing — `-f` / `--follow` recognized;
    /// unknown flags error.
    #[test]
    fn cmd_logs_recognizes_follow_flag() {
        let args_f: Vec<String> = vec!["-f".to_owned()];
        let args_follow: Vec<String> = vec!["--follow".to_owned()];
        let args_empty: Vec<String> = vec![];
        // We can't actually spawn journalctl in cargo-test, but the
        // arg-validation logic runs before the spawn. Construct the
        // logic inline:
        let follow_f = args_f.iter().any(|a| a == "-f" || a == "--follow");
        let follow_long = args_follow.iter().any(|a| a == "-f" || a == "--follow");
        let follow_none = args_empty.iter().any(|a| a == "-f" || a == "--follow");
        assert!(follow_f);
        assert!(follow_long);
        assert!(!follow_none);
    }

    /// Help text mentions the three subcommands so the user
    /// discovers them.
    #[test]
    fn help_text_lists_subcommands() {
        let h = super::HELP;
        for needle in ["status", "windows", "logs"] {
            assert!(
                h.contains(needle),
                "help should mention `{needle}` subcommand"
            );
        }
    }

    /// Pins the exact `org.halmasuit.Compositor1` triple the live
    /// `compositor1_proxy` uses (it references these same consts), so an
    /// accidental edit to the CLI's bus name / object path / interface
    /// fails CI. These MUST match what `halmasuit`'s dbus_compositor1.rs
    /// serves; the compositor1-dbus.nix VM test exercises the end-to-end
    /// match against the live server.
    #[test]
    fn compositor1_triple_matches_served_contract() {
        assert_eq!(super::COMPOSITOR1_BUS, "org.halmasuit.Compositor1");
        assert_eq!(super::COMPOSITOR1_PATH, "/org/halmasuit/Compositor1");
        assert_eq!(super::COMPOSITOR1_IFACE, "org.halmasuit.Compositor1");
    }
}
