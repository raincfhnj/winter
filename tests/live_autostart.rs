//! Opt-in live probe for the autostart machinery.
//!
//! The elevation helper is the one code path that a unit test cannot reach:
//! it hands work to a second, administrator-token process through the shell.
//! The probe below launches an elevated copy of *this* test binary with
//! `--list` — a harmless, side-effect-free libtest flag — and asserts that
//! the exit status travels back to the caller.
//!
//! Run it explicitly, from an elevated shell, on a machine where
//! administrator approval can actually be granted:
//!
//! ```text
//! cargo test --test live_autostart -- --ignored
//! ```
//!
//! It is ignored by default because a UAC prompt that nobody answers would
//! hang CI, and because the probe is only meaningful on a desktop session.

use winter::platform::windows::{
    is_current_process_elevated, run_current_process_elevated_and_wait,
};

#[test]
#[ignore = "starts an elevated child process and needs an answerable UAC prompt"]
fn the_elevation_helper_round_trips_and_reports_the_child_status() {
    assert!(
        is_current_process_elevated().expect("the access token must be readable"),
        "run this probe from an elevated shell; the child would otherwise raise a UAC prompt \
         that no automated run can answer"
    );

    let code = run_current_process_elevated_and_wait(&["--list"])
        .expect("the elevated helper must start and be waited on");

    assert_eq!(
        code, 0,
        "libtest exits successfully for --list, so a non-zero status means the exit code was \
         not propagated"
    );
}
