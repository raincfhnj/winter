use std::process::ExitCode;

fn main() -> ExitCode {
    winter::cli::run(env!("CARGO_BIN_NAME"))
}
