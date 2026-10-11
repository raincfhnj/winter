use std::env;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{Owned, PCWSTR};

use super::error::{PlatformError, PlatformResult};

/// `ERROR_CANCELLED`: the user dismissed the UAC consent dialog.
const ERROR_CANCELLED: i32 = 1223;

/// Upper bound on how long `run_current_process_elevated_and_wait` blocks.
///
/// The helper only performs a scheduled-task write, which is fast even on a
/// slow machine; two minutes is generous enough that a hit means the helper is
/// genuinely wedged (antivirus interception, hung `schtasks`) rather than slow.
const ELEVATED_HELPER_TIMEOUT_MS: u32 = 120_000;

/// Short grace period for reaping a helper that was just terminated.
const ELEVATED_HELPER_REAP_TIMEOUT_MS: u32 = 5_000;

/// Exit code reported for a helper that had to be terminated.
const ELEVATED_HELPER_TIMEOUT_EXIT_CODE: u32 = 0xDEAD;

/// The wait must be finite: `u32::MAX` is `INFINITE`, which is the unbounded
/// behaviour this constant replaced. Compile-time so a future edit that
/// reintroduces an endless wait fails the build rather than hanging a user.
const _: () = assert!(
    ELEVATED_HELPER_TIMEOUT_MS > 0 && ELEVATED_HELPER_TIMEOUT_MS < u32::MAX,
    "the elevated helper wait must be finite"
);

/// How a `WaitForSingleObject` on the elevated helper finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperOutcome {
    /// The child exited; its exit code is meaningful.
    Exited,
    /// The bounded wait elapsed; the child has to be terminated.
    TimedOut,
    /// The wait itself failed, so no exit code can be trusted.
    Failed,
}

/// Classifies a `WaitForSingleObject` result.
///
/// Kept separate from the FFI call so the decision table is testable: the
/// interesting cases (a helper that never returns, a wait that fails) cannot be
/// produced on demand against a real elevated child.
fn classify_wait(waited: windows::Win32::Foundation::WAIT_EVENT) -> HelperOutcome {
    if waited == WAIT_OBJECT_0 {
        HelperOutcome::Exited
    } else if waited == WAIT_TIMEOUT {
        HelperOutcome::TimedOut
    } else {
        HelperOutcome::Failed
    }
}

const RUN_AS_VERB: &[u16] = &[
    b'r' as u16,
    b'u' as u16,
    b'n' as u16,
    b'a' as u16,
    b's' as u16,
    0,
];

/// Returns whether this process owns a full elevated access token.
pub fn is_current_process_elevated() -> PlatformResult<bool> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle refers to this process, TOKEN_QUERY is
    // read-only, and `token` is a writable out parameter for this call.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|source| PlatformError::win32("OpenProcessToken", source))?;
    // SAFETY: OpenProcessToken returned a newly-owned valid handle.
    let token = unsafe { Owned::new(token) };

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    let size = u32::try_from(size_of::<TOKEN_ELEVATION>()).expect("TOKEN_ELEVATION fits in u32");
    // SAFETY: `elevation` has exactly the size declared to Windows and remains
    // valid and uniquely borrowed through the call; `token` stays open.
    unsafe {
        GetTokenInformation(
            *token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size,
            &mut returned,
        )
    }
    .map_err(|source| PlatformError::win32("GetTokenInformation(TokenElevation)", source))?;

    Ok(elevation.TokenIsElevated != 0)
}

/// Relaunches the current executable through UAC when it is not elevated.
///
/// `true` means an elevated successor was started and this caller must return
/// without installing hooks or dispatching any Terminal action. The successor
/// receives only the supplied command arguments; its executable path is passed
/// independently to ShellExecute and never parsed as command-line text.
pub fn relaunch_current_process_elevated(arguments: &[&str]) -> PlatformResult<bool> {
    if is_current_process_elevated()? {
        return Ok(false);
    }

    let executable =
        env::current_exe().map_err(|source| PlatformError::CurrentExecutable { source })?;
    launch_elevated(&executable, arguments)?;
    Ok(true)
}

/// Runs `arguments` in an elevated copy of this executable and waits for it.
///
/// The scheduled-task step of `winter install` must finish before the install
/// report is printed, and it cannot run in a non-elevated process. Unlike
/// [`relaunch_current_process_elevated`], which hands the whole session over
/// to the successor, this helper keeps the caller alive purely to observe the
/// outcome: it returns the child's exit status, or
/// [`PlatformError::ElevationDeclined`] when the user dismisses the prompt.
///
/// Callers must check [`is_current_process_elevated`] first; an elevated
/// caller would otherwise spawn a pointless child.
pub fn run_current_process_elevated_and_wait(arguments: &[&str]) -> PlatformResult<u32> {
    let executable =
        env::current_exe().map_err(|source| PlatformError::CurrentExecutable { source })?;
    let executable = wide_null_os(executable.as_os_str());
    let parameters = command_line_parameters(arguments);
    let parameters = wide_null(&parameters);
    let verb = RUN_AS_VERB;

    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>())
            .expect("SHELLEXECUTEINFOW fits in u32"),
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..SHELLEXECUTEINFOW::default()
    };

    // SAFETY: `info` is fully initialized with a valid `cbSize`, the UTF-16
    // buffers stay alive across the call, and SEE_MASK_NOCLOSEPROCESS asks
    // Windows to return an owned process handle in `hProcess`.
    unsafe { ShellExecuteExW(&mut info) }.map_err(elevation_error)?;

    // SAFETY: SEE_MASK_NOCLOSEPROCESS guarantees a valid handle on success.
    let process = unsafe { Owned::new(info.hProcess) };
    // SAFETY: `process` is a live process handle owned by this scope.
    let waited = unsafe { WaitForSingleObject(*process, ELEVATED_HELPER_TIMEOUT_MS) };
    match classify_wait(waited) {
        HelperOutcome::Exited => {}
        HelperOutcome::TimedOut => {
            // A helper that never returns must not hang `winter install` or
            // `winter uninstall` forever: waiting on it with `INFINITE` had no
            // cancellation path at all, so an antivirus stall or a wedged
            // `schtasks` left the parent process stuck with no way out.
            //
            // The child is terminated so the caller is not left with a rogue
            // elevated process still writing the task definition, and the error
            // says the operation may be partial rather than claiming success.
            // SAFETY: `process` is a live handle owned by this scope; the exit
            // code is irrelevant because the caller is told the helper was
            // killed.
            let _ = unsafe { TerminateProcess(*process, ELEVATED_HELPER_TIMEOUT_EXIT_CODE) };
            // SAFETY: the handle is still valid; this reaps the terminated child
            // so the caller is not left waiting on a zombie.
            unsafe { WaitForSingleObject(*process, ELEVATED_HELPER_REAP_TIMEOUT_MS) };
            return Err(PlatformError::ElevationHelperTimeout {
                seconds: u64::from(ELEVATED_HELPER_TIMEOUT_MS) / 1_000,
            });
        }
        HelperOutcome::Failed => {
            // The wait itself did not observe the child exit, so reading an
            // exit code would report whatever the process happens to have. Say
            // the wait failed instead of inventing a status.
            return Err(PlatformError::win32(
                "WaitForSingleObject",
                windows::core::Error::from_thread(),
            ));
        }
    }
    let mut code = 0_u32;
    // SAFETY: `process` is a live process handle; `code` is a writable out
    // parameter of the documented type.
    unsafe { GetExitCodeProcess(*process, &mut code) }
        .map_err(|source| PlatformError::win32("GetExitCodeProcess", source))?;
    // `process` is an owned handle; it is closed when it drops here, so no
    // explicit CloseHandle is required.

    Ok(code)
}

/// Maps a `ShellExecuteExW` failure onto a dismissal or a generic failure.
fn elevation_error(source: windows::core::Error) -> PlatformError {
    if source.code().0 == ERROR_CANCELLED {
        PlatformError::ElevationDeclined
    } else {
        PlatformError::win32("ShellExecuteExW", source)
    }
}

fn launch_elevated(executable: &Path, arguments: &[&str]) -> PlatformResult<()> {
    let executable = wide_null_os(executable.as_os_str());
    let parameters = command_line_parameters(arguments);
    let parameters = wide_null(&parameters);

    // SAFETY: the UTF-16 buffers remain NUL-terminated and alive throughout
    // the call. ShellExecute only receives immutable pointers and transfers no
    // ownership back to this process.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(RUN_AS_VERB.as_ptr()),
            PCWSTR(executable.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    let result_code = result.0 as isize;
    if result_code <= 32 {
        Err(PlatformError::ElevationLaunch { result_code })
    } else {
        Ok(())
    }
}

fn command_line_parameters(arguments: &[&str]) -> String {
    arguments
        .iter()
        .map(|argument| quote_windows_command_line_argument(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_windows_command_line_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn wide_null_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_FAILED};

    use super::{
        ELEVATED_HELPER_TIMEOUT_MS, HelperOutcome, WAIT_OBJECT_0, WAIT_TIMEOUT, classify_wait,
        command_line_parameters, quote_windows_command_line_argument,
    };

    #[test]
    fn quotes_relaunch_arguments_with_windows_rules() {
        assert_eq!(quote_windows_command_line_argument("run"), "run");
        assert_eq!(
            quote_windows_command_line_argument("two words"),
            "\"two words\""
        );
        assert_eq!(
            quote_windows_command_line_argument("a\\\"b"),
            "\"a\\\\\\\"b\""
        );
        assert_eq!(
            quote_windows_command_line_argument("tail path\\"),
            "\"tail path\\\\\""
        );
    }

    #[test]
    fn joins_individually_quoted_relaunch_arguments() {
        assert_eq!(
            command_line_parameters(&["run", "--no-launch", "中文 path"]),
            "run --no-launch \"中文 path\""
        );
    }

    /// The helper is only ever waited on with a finite timeout: an unbounded
    /// wait would hang `winter install` / `winter uninstall` with no way out
    /// when the elevated child wedges. The bound itself is asserted at compile
    /// time (see the `const _` next to the constant).
    #[test]
    fn the_elevated_helper_wait_is_bounded() {
        assert_eq!(
            ELEVATED_HELPER_TIMEOUT_MS, 120_000,
            "two minutes: generous for a task write, short enough to notice a wedged helper"
        );
    }

    #[test]
    fn wait_results_map_to_exit_timeout_and_failure() {
        assert_eq!(classify_wait(WAIT_OBJECT_0), HelperOutcome::Exited);
        assert_eq!(classify_wait(WAIT_TIMEOUT), HelperOutcome::TimedOut);
        // WAIT_FAILED and WAIT_ABANDONED both mean "no exit code can be trusted".
        assert_eq!(
            classify_wait(WAIT_FAILED),
            HelperOutcome::Failed,
            "a failed wait must not be read as a clean exit"
        );
        assert_eq!(classify_wait(WAIT_ABANDONED), HelperOutcome::Failed);
    }
}
