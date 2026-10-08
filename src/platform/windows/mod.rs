//! Narrow Win32 adapter for Windows Terminal discovery and keyboard control.
//!
//! This module deliberately exposes intent-neutral primitives. Prefix parsing
//! and action routing belong to the controller layer, not to the platform
//! callbacks.

mod accessibility;
mod cursor;
mod dpi;
mod elevation;
mod error;
mod foreground;
mod hook;
mod input;
mod known_folder;
mod launcher;
mod single_instance;

pub use accessibility::{TabInfo, TerminalAccessibility};
pub use cursor::show_pane_resize_cursor;
pub use dpi::enable_per_monitor_dpi_awareness;
pub use elevation::{is_current_process_elevated, relaunch_current_process_elevated};
pub use error::{ModifierKey, PlatformError, PlatformResult};
pub use foreground::{
    foreground_hwnd, foreground_terminal_window, terminal_window_identity, validate_window_identity,
};
pub use hook::{
    CONTROLLER_INPUT_MARKER, HookDecision, InputHook, KeyTransition, MouseEventKind, RawInputEvent,
    RawInputHandler, RawKeyEvent, RawMouseEvent,
};
pub(crate) use input::key_is_down;
pub use input::{InputDispatch, send_bridge_chord, send_literal_chord};
pub use known_folder::documents_directory;
pub use launcher::launch_windows_terminal;
pub use single_instance::{DEFAULT_INSTANCE_MUTEX_NAME, SingleInstanceGuard};
