use std::sync::OnceLock;

use windows::Win32::UI::WindowsAndMessaging::{
    HCURSOR, IDC_SIZENS, IDC_SIZEWE, LoadCursorW, SetCursor,
};
use windows::core::PCWSTR;

use crate::pane_layout::SplitAxis;

use super::error::{PlatformError, PlatformResult};

static SIZE_WE_CURSOR: OnceLock<usize> = OnceLock::new();
static SIZE_NS_CURSOR: OnceLock<usize> = OnceLock::new();

/// Displays the standard Windows resize cursor for a native pane divider.
///
/// The controller uses this only after it has captured a primary-button drag.
/// Ordinary hover moves always pass through to Windows Terminal so the cursor
/// hint can never block the terminal's pointer input.
///
/// The underlying `HCURSOR` handles are loaded once per axis and cached for
/// the lifetime of the process.
pub fn show_pane_resize_cursor(axis: SplitAxis) -> PlatformResult<()> {
    let (cache, resource) = match axis {
        SplitAxis::Vertical => (&SIZE_WE_CURSOR, IDC_SIZEWE),
        SplitAxis::Horizontal => (&SIZE_NS_CURSOR, IDC_SIZENS),
    };
    let cursor = load_cached_cursor(cache, resource)?;
    // SAFETY: `cursor` is a live shared system cursor handle.
    unsafe {
        let _ = SetCursor(Some(cursor));
    }
    Ok(())
}

fn load_cached_cursor(cache: &OnceLock<usize>, resource: PCWSTR) -> PlatformResult<HCURSOR> {
    let handle = match cache.get() {
        Some(&handle) => handle,
        None => {
            // SAFETY: predefined cursor resources are process-independent
            // shared handles and must not be destroyed by the caller.
            let cursor = unsafe { LoadCursorW(None, resource) }
                .map_err(|source| PlatformError::win32("LoadCursorW(resize)", source))?;
            *cache.get_or_init(|| cursor.0 as usize)
        }
    };

    Ok(HCURSOR(handle as *mut core::ffi::c_void))
}
