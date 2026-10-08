//! Short command-name entry point for Winter.
//!
//! Shares the exact CLI implementation with `winterminalp.exe`; only the
//! displayed binary name differs.

use std::process::ExitCode;

fn main() -> ExitCode {
    winter::cli::run(env!("CARGO_BIN_NAME"))
}
