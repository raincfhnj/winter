use std::io;

use thiserror::Error;

use crate::model::WindowIdentity;

pub type PlatformResult<T> = Result<T, PlatformError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Control,
    Alt,
    Shift,
    Windows,
}

impl std::fmt::Display for ModifierKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Control => "control",
            Self::Alt => "alt",
            Self::Shift => "shift",
            Self::Windows => "windows",
        })
    }
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("{operation} failed: {source}")]
    Win32 {
        operation: &'static str,
        #[source]
        source: windows::core::Error,
    },

    #[error("a low-level input hook is already active in this process")]
    HookAlreadyActive,

    #[error("the low-level input hook thread stopped before initialization")]
    HookStartupTerminated,

    #[error("failed to spawn the low-level input hook thread: {source}")]
    HookThreadSpawn {
        #[source]
        source: io::Error,
    },

    #[error("the low-level input hook thread panicked")]
    HookThreadPanicked,

    #[error("the low-level input hook thread did not exit within the shutdown timeout")]
    HookShutdownTimeout,

    #[error(
        "the target window is not the foreground window (expected {expected:#x}, actual {actual:#x})"
    )]
    TargetNotForeground { expected: isize, actual: isize },

    #[error("the Windows Terminal target changed (expected {expected:?}, actual {actual:?})")]
    TargetWindowChanged {
        expected: WindowIdentity,
        actual: Option<WindowIdentity>,
    },

    #[error("bridge function key must be in the F13-F24 range, got F{0}")]
    InvalidBridgeFunctionKey(u8),

    #[error("unexpected physical {0} modifier is held; refusing to alter user keyboard state")]
    UnexpectedModifierHeld(ModifierKey),

    #[error(
        "expected physical {0} modifier is no longer held; keyboard state changed before injection"
    )]
    ModifierReleased(ModifierKey),

    #[error("target key (virtual key {0:#x}) is already physically held")]
    TargetKeyHeld(u16),

    #[error("no Windows Terminal pane contains screen point ({x}, {y}) in HWND {hwnd:#x}")]
    PaneNotFoundAt { hwnd: isize, x: i32, y: i32 },

    #[error("input sequence contains too many events for SendInput: {0}")]
    InputSequenceTooLarge(usize),

    #[error("INPUT structure size cannot be represented for SendInput: {0} bytes")]
    InputStructureSizeTooLarge(usize),

    #[error(
        "SendInput inserted {sent}/{expected} events; cleanup inserted {cleanup_sent}/{cleanup_expected}; OS error {os_error}; UIPI may have blocked input"
    )]
    InputInjectionIncomplete {
        sent: u32,
        expected: u32,
        cleanup_sent: u32,
        cleanup_expected: u32,
        os_error: u32,
    },

    #[error("another Winter controller instance is already running")]
    AlreadyRunning,

    #[error("invalid named mutex: {0}")]
    InvalidMutexName(String),

    #[error("failed to launch wt.exe: {source}")]
    TerminalLaunch {
        #[source]
        source: io::Error,
    },

    #[error("failed to resolve the current executable for elevation: {source}")]
    CurrentExecutable {
        #[source]
        source: io::Error,
    },

    #[error("Windows refused the elevated controller launch (ShellExecute result {result_code})")]
    ElevationLaunch { result_code: isize },
}

impl PlatformError {
    #[must_use]
    pub(crate) fn win32(operation: &'static str, source: windows::core::Error) -> Self {
        Self::Win32 { operation, source }
    }
}
