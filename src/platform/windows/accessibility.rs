use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomation2, IUIAutomationElement, TreeScope_Descendants,
    UIA_ClassNamePropertyId,
};
use windows::core::Interface;

use crate::model::WindowIdentity;
use crate::pane_layout::{PaneGeometry, ScreenPoint, ScreenRect};

use super::error::{PlatformError, PlatformResult};
use super::foreground::{foreground_hwnd, validate_window_identity};

const UI_AUTOMATION_CONNECTION_TIMEOUT_MS: u32 = 250;
const UI_AUTOMATION_TRANSACTION_TIMEOUT_MS: u32 = 250;
const TERMINAL_CONTROL_CLASS_NAME: &str = "TermControl";

/// Thread-affine UI Automation client used only outside low-level hook callbacks.
///
/// Windows Terminal remains the layout authority. This adapter reads disposable
/// `TermControl` rectangles and can focus one native pane before dispatching a
/// documented `resizePane` action; it never persists or reconstructs a pane tree.
pub struct TerminalAccessibility {
    automation: IUIAutomation,
    _apartment: ComApartment,
}

impl TerminalAccessibility {
    pub fn initialize() -> PlatformResult<Self> {
        let apartment = ComApartment::initialize()?;
        // SAFETY: COM is initialized on this thread, CUIAutomation is an
        // in-process COM class, and no aggregation is requested.
        let automation = unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        }
        .map_err(|source| PlatformError::win32("CoCreateInstance(CUIAutomation)", source))?;

        if let Ok(automation_v2) = automation.cast::<IUIAutomation2>() {
            // SAFETY: the interface was queried from the live automation
            // object and the timeout values are finite milliseconds.
            unsafe {
                let _ = automation_v2.SetConnectionTimeout(UI_AUTOMATION_CONNECTION_TIMEOUT_MS);
                let _ = automation_v2.SetTransactionTimeout(UI_AUTOMATION_TRANSACTION_TIMEOUT_MS);
            }
        }

        Ok(Self {
            automation,
            _apartment: apartment,
        })
    }

    pub fn pane_geometries(&self, hwnd: isize) -> PlatformResult<Vec<PaneGeometry>> {
        let elements = self.terminal_elements(hwnd)?;
        let element_count = elements.len();
        let mut panes = Vec::with_capacity(element_count);
        let mut first_error = None;
        for element in elements {
            // A single stale element must not discard the whole snapshot; the
            // caller rebuilds the layout on the next poll anyway.
            // SAFETY: `element` is a live UI Automation proxy used on the COM
            // apartment where it was obtained.
            let bounds = match unsafe { element.CurrentBoundingRectangle() } {
                Ok(bounds) => bounds,
                Err(source) => {
                    first_error.get_or_insert_with(|| {
                        PlatformError::win32(
                            "IUIAutomationElement::CurrentBoundingRectangle",
                            source,
                        )
                    });
                    continue;
                }
            };
            let has_keyboard_focus = match unsafe { element.CurrentHasKeyboardFocus() } {
                Ok(value) => value,
                Err(source) => {
                    first_error.get_or_insert_with(|| {
                        PlatformError::win32(
                            "IUIAutomationElement::CurrentHasKeyboardFocus",
                            source,
                        )
                    });
                    continue;
                }
            };
            // Third cross-process property read, purely informational: an
            // element that cannot report its name still contributes its
            // rectangle, so this failure neither drops the pane nor seeds
            // `first_error` (the "only fail when everything failed" policy
            // stays keyed to the bounding-rect/focus reads above).
            let bounds = ScreenRect::new(bounds.left, bounds.top, bounds.right, bounds.bottom);
            if bounds.is_valid() {
                // SAFETY: `element` is a live UI Automation proxy used on the
                // COM apartment where it was obtained.
                let title = match unsafe { element.CurrentName() } {
                    Ok(name) => String::from_utf16_lossy(&name),
                    Err(_) => String::new(),
                };
                panes.push(PaneGeometry {
                    bounds,
                    has_keyboard_focus: has_keyboard_focus.as_bool(),
                    title,
                });
            }
        }
        // Only surface a failure when every enumerated control was unusable;
        // otherwise a transient stale element would discard a valid snapshot.
        if panes.is_empty()
            && element_count > 0
            && let Some(error) = first_error
        {
            return Err(error);
        }
        Ok(panes)
    }

    pub fn focus_pane_at(&self, target: WindowIdentity, point: ScreenPoint) -> PlatformResult<()> {
        let actual = foreground_hwnd();
        if actual != target.hwnd {
            return Err(PlatformError::TargetNotForeground {
                expected: target.hwnd,
                actual,
            });
        }
        validate_window_identity(target)?;
        let element = self
            .terminal_elements(target.hwnd)?
            .into_iter()
            .find_map(|element| {
                // A transient provider failure for one stale element should
                // not prevent a later live TermControl from being selected.
                let bounds = unsafe { element.CurrentBoundingRectangle() }.ok()?;
                ScreenRect::new(bounds.left, bounds.top, bounds.right, bounds.bottom)
                    .contains(point)
                    .then_some(element)
            })
            .ok_or(PlatformError::PaneNotFoundAt {
                hwnd: target.hwnd,
                x: point.x,
                y: point.y,
            })?;

        // SAFETY: `element` is a TermControl descendant of the revalidated
        // target HWND and is used on its creating COM apartment.
        unsafe { element.SetFocus() }
            .map_err(|source| PlatformError::win32("IUIAutomationElement::SetFocus", source))
    }

    fn terminal_elements(&self, hwnd: isize) -> PlatformResult<Vec<IUIAutomationElement>> {
        // SAFETY: the caller supplies an opaque top-level HWND that is checked
        // by UI Automation; invalid or stale handles become a structured error.
        let root = unsafe { self.automation.ElementFromHandle(isize_to_hwnd(hwnd)) }
            .map_err(|source| PlatformError::win32("IUIAutomation::ElementFromHandle", source))?;
        // Filtering by class name in the provider avoids marshaling every
        // descendant across the process boundary just to discard it.
        // SAFETY: the property id and string value are valid for this client,
        // and the temporary VARIANT is copied into the condition before it is
        // cleared by its `Drop` implementation.
        let condition = unsafe {
            self.automation.CreatePropertyCondition(
                UIA_ClassNamePropertyId,
                &VARIANT::from(TERMINAL_CONTROL_CLASS_NAME),
            )
        }
        .map_err(|source| PlatformError::win32("IUIAutomation::CreatePropertyCondition", source))?;
        // SAFETY: `root` and `condition` are live proxies on this apartment;
        // descendants are read-only accessibility elements.
        let descendants = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
            .map_err(|source| PlatformError::win32("IUIAutomationElement::FindAll", source))?;
        let length = unsafe { descendants.Length() }
            .map_err(|source| PlatformError::win32("IUIAutomationElementArray::Length", source))?;
        let mut elements = Vec::with_capacity(length as usize);
        for index in 0..length {
            // A single transiently unavailable element must not fail the whole
            // enumeration; callers skip the missing pane.
            if let Ok(element) = unsafe { descendants.GetElement(index) } {
                elements.push(element);
            }
        }
        Ok(elements)
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> PlatformResult<Self> {
        // SAFETY: this is called once for a newly spawned worker thread and is
        // balanced by CoUninitialize when the apartment guard is dropped.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|source| PlatformError::win32("CoInitializeEx", source))?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: this guard is dropped on the same thread where initialization
        // succeeded, after the automation field has been released.
        unsafe { CoUninitialize() };
    }
}

fn isize_to_hwnd(hwnd: isize) -> HWND {
    HWND(hwnd as *mut core::ffi::c_void)
}
