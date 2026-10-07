use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, SetLastError, WIN32_ERROR};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

/// Opts the controller into per-monitor DPI awareness.
///
/// Low-level mouse hook coordinates are physical pixels, while UI Automation
/// virtualizes bounding rectangles for DPI-unaware callers. Without this the
/// inferred divider coordinates would not line up with the cursor on scaled
/// displays.
///
/// Returns `true` when the process is per-monitor aware afterwards: either the
/// call succeeded, or it failed with `ERROR_ACCESS_DENIED` because a manifest
/// or an earlier call already fixed the awareness (also a valid outcome).
/// Returns `false` for any other failure, in which case hook coordinates and
/// UIA rectangles disagree on HiDPI displays.
pub fn enable_per_monitor_dpi_awareness() -> bool {
    // SAFETY: resetting the calling thread's last-error value does not
    // dereference pointers or transfer ownership.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    // SAFETY: this takes a constant enum by value and has no pointer or
    // ownership requirements.
    let result =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: this reads the calling thread's last-error value immediately
    // after the call, before anything else can overwrite it.
    let last_error = unsafe { GetLastError() }.0;
    classify(result.is_ok(), last_error)
}

/// Maps a `SetProcessDpiAwarenessContext` outcome plus the last-error code
/// read straight after it onto the reported success flag.
///
/// `ERROR_ACCESS_DENIED` means the awareness was already fixed elsewhere,
/// which leaves the process in the intended state and counts as success.
fn classify(result_ok: bool, last_error: u32) -> bool {
    result_ok || last_error == ERROR_ACCESS_DENIED.0
}

#[cfg(test)]
mod tests {
    use super::{ERROR_ACCESS_DENIED, classify};

    #[test]
    fn successful_call_reports_enabled() {
        assert!(classify(true, 0));
        assert!(classify(true, ERROR_ACCESS_DENIED.0));
    }

    #[test]
    fn access_denied_means_awareness_already_set() {
        assert!(classify(false, ERROR_ACCESS_DENIED.0));
        assert!(classify(false, 5));
    }

    #[test]
    fn other_failures_report_disabled() {
        assert!(!classify(false, 0));
        assert!(!classify(false, 87));
        assert!(!classify(false, u32::MAX));
    }
}
