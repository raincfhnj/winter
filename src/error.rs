use std::path::PathBuf;

use thiserror::Error;

use crate::platform::windows::PlatformError;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Winter only supports Windows")]
    UnsupportedPlatform,

    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Windows Terminal is not installed or wt.exe is unavailable")]
    TerminalNotInstalled,

    #[error("another Winter controller instance is already running")]
    ControllerAlreadyRunning,

    #[error("the controller must run elevated to control an elevated Windows Terminal")]
    ControllerRequiresElevation,

    /// User-resolvable content conflict: the settings, fragment, or manifest
    /// on disk diverged from what this operation expected (including
    /// concurrent user edits detected by compare-and-swap checks), so the
    /// user must inspect and resolve the conflicting content.
    #[error("Windows Terminal settings conflict: {0}")]
    SettingsConflict(String),

    /// The operation could not be carried through to completion — a rollback
    /// did not restore everything, a persistence step failed, or the
    /// operation observed an impossible mid-flight state. The system may be
    /// partially modified; raw backups and retained manifests describe what
    /// is left. This is a tool-side incompleteness, not a content conflict.
    #[error("Windows Terminal settings operation incomplete: {0}")]
    OperationIncomplete(String),

    #[error("Windows Terminal settings error at {path}: {message}")]
    Settings { path: PathBuf, message: String },

    /// A Windows API failure wrapped with its [`PlatformError`] source chain
    /// intact, so `Error::source()` reaches the underlying cause instead of
    /// dropping it into a flattened string. `Display` renders exactly as the
    /// historical `Native(platform_error.to_string())` did, with `context`
    /// appended verbatim when a caller attaches diagnostics.
    #[error("native Windows operation failed: {source}{context}")]
    Platform {
        #[source]
        source: PlatformError,
        context: String,
    },

    /// Free-form diagnostic message with no structured source.
    #[error("native Windows operation failed: {0}")]
    Native(String),

    #[error("I/O operation {operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl AppError {
    #[must_use]
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    /// Wraps a [`PlatformError`] while preserving its source chain.
    #[must_use]
    pub fn platform(source: PlatformError) -> Self {
        Self::Platform {
            source,
            context: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::platform::windows::PlatformError;

    #[test]
    fn platform_errors_keep_their_source_chain() {
        let error = AppError::platform(PlatformError::HookStartupTerminated);

        let source = Error::source(&error).expect("PlatformError must be reachable as source");
        assert_eq!(
            source.to_string(),
            "the low-level input hook thread stopped before initialization"
        );
        assert_eq!(
            error.to_string(),
            "native Windows operation failed: the low-level input hook thread stopped before initialization"
        );
    }

    #[test]
    fn platform_context_is_rendered_after_the_source_message() {
        let error = AppError::Platform {
            source: PlatformError::HookThreadPanicked,
            context:
                "; action worker also recorded 1 failed action(s), last error: injection failed"
                    .to_owned(),
        };

        let rendered = error.to_string();
        assert!(
            rendered.starts_with(
                "native Windows operation failed: the low-level input hook thread panicked; "
            ),
            "unexpected rendering: {rendered}"
        );
        assert!(rendered.ends_with("last error: injection failed"));
        assert!(Error::source(&error).is_some());
    }

    #[test]
    fn operation_incomplete_labels_an_incomplete_operation() {
        let error = AppError::OperationIncomplete("rollback did not finish".to_owned());

        assert_eq!(
            error.to_string(),
            "Windows Terminal settings operation incomplete: rollback did not finish"
        );
        assert!(
            !error.to_string().contains("settings conflict"),
            "incomplete operations must not masquerade as user conflicts"
        );
    }
}
