//! Fuzz target for halmasuit-spawn's argv parser.
//!
//! The fuzzer's invariant: `parse_argv` must never panic, regardless of
//! input shape. Any panic libfuzzer finds is a real bug — the parser
//! consumes attacker-influenced argv (the compositor's spawn() call is
//! a known compromise vector per ARCHITECTURE.md threat model row 11).
//!
//! Input shaping: we split the raw fuzz bytes on NUL so libfuzzer's
//! coverage-guided search shapes the input as an argv vector, mirroring
//! how the kernel hands argv to execve(2). This lets the fuzzer discover
//! both per-arg byte patterns and overall argv structure without burning
//! cycles re-discovering the separator.

#![no_main]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let args: Vec<OsString> = data
        .split(|&b| b == 0)
        .map(|chunk| OsString::from_vec(chunk.to_vec()))
        .collect();
    let _ = halmasuit_spawn::parse_argv(args);
});
