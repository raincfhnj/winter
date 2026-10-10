use std::env;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{Owned, PCWSTR};

use super::error::{PlatformError, PlatformResult};

/// `ERROR_CANCELLED`: the user dismissed the UAC consent dialog.
const ERROR_CANCELLED: i32 = 1223;

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
    unsafe { WaitForSingleObject(*process, u32::MAX) };
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
    use super::{command_line_parameters, quote_windows_command_line_argument};

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
}
