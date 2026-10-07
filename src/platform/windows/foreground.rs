use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, HWND};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::{Error, Owned, PWSTR};

use crate::model::{TerminalChannel, WindowIdentity};

use super::error::{PlatformError, PlatformResult};

const INITIAL_PROCESS_PATH_CHARS: usize = 512;
const MAX_PROCESS_PATH_CHARS: usize = 32_768;

/// Returns the foreground window when it belongs to a Windows Terminal host.
///
/// `Ok(None)` is the normal result when another application has focus or while
/// Windows is between foreground windows.
pub fn foreground_terminal_window() -> PlatformResult<Option<WindowIdentity>> {
    // SAFETY: GetForegroundWindow takes no pointers and returns a borrowed HWND
    // that is validated before any process query.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return Ok(None);
    }

    terminal_window_identity(hwnd_to_isize(hwnd))
}

/// Resolves a top-level HWND to a stable Windows Terminal identity.
///
/// The executable basename must be exactly `WindowsTerminal.exe`
/// (case-insensitive). Process creation time is included to prevent PID/HWND
/// reuse from validating a stale target.
pub fn terminal_window_identity(hwnd: isize) -> PlatformResult<Option<WindowIdentity>> {
    let hwnd = isize_to_hwnd(hwnd);
    if hwnd.is_invalid() {
        return Ok(None);
    }

    let mut process_id = 0;
    // SAFETY: `hwnd` is only used as an opaque handle and `process_id` is a
    // valid writable out parameter for the duration of the call.
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if thread_id == 0 || process_id == 0 {
        return Ok(None);
    }

    let process = open_process_for_identity(process_id)?;
    let image_path = process_image_path(*process)?;
    let Some(channel) = terminal_channel_for_path(&image_path) else {
        return Ok(None);
    };
    let process_started_at_100ns = process_creation_time(*process)?;

    Ok(Some(WindowIdentity {
        hwnd: hwnd_to_isize(hwnd),
        process_id,
        process_started_at_100ns,
        channel,
    }))
}

/// Rechecks HWND, PID, process creation time, and distribution channel.
pub fn validate_window_identity(expected: WindowIdentity) -> PlatformResult<()> {
    let actual = terminal_window_identity(expected.hwnd)?;
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(PlatformError::TargetWindowChanged { expected, actual })
    }
}

/// Cheap, non-blocking foreground HWND snapshot suitable for a hook callback.
///
/// This performs no process access, allocation, or I/O. A zero value means
/// Windows has no foreground window at this instant.
#[must_use]
pub fn foreground_hwnd() -> isize {
    // SAFETY: GetForegroundWindow has no pointer arguments; a stale/zero HWND
    // is only treated as a snapshot and is fully revalidated before dispatch.
    hwnd_to_isize(unsafe { GetForegroundWindow() })
}

fn open_process_for_identity(process_id: u32) -> PlatformResult<Owned<HANDLE>> {
    // SAFETY: the access mask is read-only and the returned handle is checked
    // by the windows projection before ownership is assumed.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map_err(|source| PlatformError::win32("OpenProcess", source))?;

    // SAFETY: `handle` is a newly opened, valid, uniquely owned process handle.
    Ok(unsafe { Owned::new(handle) })
}

fn process_image_path(process: HANDLE) -> PlatformResult<PathBuf> {
    // Almost every image path fits in the first buffer; only fall back to the
    // documented maximum when Windows reports that it was too small.
    for capacity in [INITIAL_PROCESS_PATH_CHARS, MAX_PROCESS_PATH_CHARS] {
        let mut buffer = vec![0_u16; capacity];
        let mut length = u32::try_from(capacity).expect("path capacity fits in u32");

        // SAFETY: `buffer` is writable for `capacity` UTF-16 units, `length`
        // describes that capacity, and `process` is valid while borrowed.
        let result = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        match result {
            Ok(()) => {
                buffer.truncate(length as usize);
                return Ok(PathBuf::from(OsString::from_wide(&buffer)));
            }
            Err(source)
                if capacity == INITIAL_PROCESS_PATH_CHARS
                    && is_insufficient_capacity(&source, length, capacity) =>
            {
                continue;
            }
            Err(source) => {
                return Err(PlatformError::win32("QueryFullProcessImageNameW", source));
            }
        }
    }
    unreachable!("the final loop iteration returns on success or failure")
}

/// Reports whether a failed `QueryFullProcessImageNameW` call failed only
/// because the buffer was too small, i.e. whether retrying with a larger
/// buffer can succeed.
fn is_insufficient_capacity(source: &Error, reported_length: u32, capacity: usize) -> bool {
    source.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() || reported_length as usize > capacity
}

fn process_creation_time(process: HANDLE) -> PlatformResult<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY: all FILETIME out parameters are valid and uniquely borrowed for
    // the call, and `process` remains valid while this function executes.
    unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|source| PlatformError::win32("GetProcessTimes", source))?;

    Ok(filetime_to_u64(creation))
}

const PACKAGED_CHANNEL_MARKERS: [(&[u8], TerminalChannel); 3] = [
    (
        b"microsoft.windowsterminalpreview_",
        TerminalChannel::Preview,
    ),
    (b"microsoft.windowsterminalcanary_", TerminalChannel::Canary),
    (b"microsoft.windowsterminal_", TerminalChannel::Stable),
];

fn terminal_channel_for_path(path: &Path) -> Option<TerminalChannel> {
    if !eq_ignore_ascii_case(path.file_name()?.as_encoded_bytes(), b"WindowsTerminal.exe") {
        return None;
    }

    let raw_path = path.as_os_str().as_encoded_bytes();
    for (marker, channel) in PACKAGED_CHANNEL_MARKERS {
        if contains_ascii_case_insensitive(raw_path, marker) {
            return Some(channel);
        }
    }

    let portable_marker = path.parent().map(|parent| parent.join(".portable"));
    if portable_marker.as_deref().is_some_and(Path::is_file) {
        Some(TerminalChannel::Portable)
    } else {
        Some(TerminalChannel::Unpackaged)
    }
}

fn eq_ignore_ascii_case(actual: &[u8], expected: &[u8]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| eq_ignore_ascii_case(window, needle))
}

const fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn isize_to_hwnd(hwnd: isize) -> HWND {
    HWND(hwnd as *mut core::ffi::c_void)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn classifies_packaged_terminal_channels() {
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.24.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
            )),
            Some(TerminalChannel::Stable)
        );
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminalPreview_1.25.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
            )),
            Some(TerminalChannel::Preview)
        );
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminalCanary_1.26.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
            )),
            Some(TerminalChannel::Canary)
        );
    }

    #[test]
    fn classifies_channels_case_insensitively_without_normalized_copy() {
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"c:\program files\windowsapps\MICROSOFT.WINDOWSTERMINALPREVIEW_1.25.0_x64__8wekyb3d8bbwe\Windowsterminal.EXE"
            )),
            Some(TerminalChannel::Preview)
        );
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"C:/Program Files/WindowsApps/Microsoft.WindowsTerminalCanary_1.26.0_x64__8wekyb3d8bbwe/WindowsTerminal.exe"
            )),
            Some(TerminalChannel::Canary)
        );
        assert_eq!(
            terminal_channel_for_path(Path::new(
                r"C:\Проекты\Microsoft.WindowsTerminal_1.24.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe"
            )),
            Some(TerminalChannel::Stable)
        );
    }

    #[test]
    fn ascii_case_helpers_match_only_expected_patterns() {
        assert!(eq_ignore_ascii_case(
            b"WindowsTerminal.exe",
            b"windowsterminal.EXE"
        ));
        assert!(!eq_ignore_ascii_case(
            b"WindowsTerminal.exe",
            b"WindowsTerminal.exe "
        ));
        assert!(!eq_ignore_ascii_case(
            b"WindowsTerminal.ex",
            b"WindowsTerminal.exe"
        ));

        let haystack = b"C:\\Apps\\Microsoft.WindowsTerminalPreview_1.0\\wt.exe";
        assert!(contains_ascii_case_insensitive(
            haystack,
            b"microsoft.windowsterminalpreview_"
        ));
        assert!(contains_ascii_case_insensitive(
            haystack,
            b"MICROSOFT.windowsterminalPREVIEW_"
        ));
        assert!(!contains_ascii_case_insensitive(
            haystack,
            b"microsoft.windowsterminalcanary_"
        ));
        assert!(!contains_ascii_case_insensitive(b"short", b"longer_needle"));
        assert!(contains_ascii_case_insensitive(b"anything", b""));
    }

    #[test]
    fn retries_path_query_only_when_buffer_was_too_small() {
        let capacity = INITIAL_PROCESS_PATH_CHARS as u32;
        let insufficient = Error::from_hresult(ERROR_INSUFFICIENT_BUFFER.to_hresult());
        let access_denied =
            Error::from_hresult(windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult());

        assert!(is_insufficient_capacity(
            &insufficient,
            capacity,
            capacity as usize
        ));
        assert!(
            !is_insufficient_capacity(&access_denied, capacity, capacity as usize),
            "a real failure must propagate instead of retrying"
        );
        assert!(is_insufficient_capacity(
            &access_denied,
            capacity + 1,
            capacity as usize
        ));
    }

    #[test]
    fn requires_exact_terminal_executable_name() {
        assert_eq!(
            terminal_channel_for_path(Path::new(r"C:\Tools\WindowsTerminal-helper.exe")),
            None
        );
        assert_eq!(
            terminal_channel_for_path(Path::new(r"C:\Tools\WindowsTerminal.exe")),
            Some(TerminalChannel::Unpackaged)
        );
    }

    #[test]
    fn recognizes_portable_marker() {
        let directory = tempdir().expect("create temp directory");
        fs::write(directory.path().join(".portable"), []).expect("create marker");
        let executable = directory.path().join("WindowsTerminal.exe");

        assert_eq!(
            terminal_channel_for_path(&executable),
            Some(TerminalChannel::Portable)
        );
    }

    #[test]
    fn combines_filetime_halves() {
        assert_eq!(
            filetime_to_u64(FILETIME {
                dwLowDateTime: 0x89ab_cdef,
                dwHighDateTime: 0x0123_4567,
            }),
            0x0123_4567_89ab_cdef
        );
    }
}
