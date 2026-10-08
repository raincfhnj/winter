//! Shared helpers for settings integration: desired bindings, error context,
//! channel labels, and target de-duplication.

use std::collections::HashSet;
use std::path::Path;

use crate::keymap::managed_bindings;
use crate::{AppError, AppResult, TerminalChannel};

use super::jsonc::{DesiredKeybinding, desired_keybinding, validate_desired_bindings};
use super::types::{TerminalSettingsTarget, path_key};

pub(super) fn desired_keybindings() -> AppResult<Vec<DesiredKeybinding>> {
    let desired = managed_bindings()
        .iter()
        .copied()
        .map(|binding| desired_keybinding(binding.keybinding_definition_json()))
        .collect::<AppResult<Vec<_>>>()?;
    validate_desired_bindings(&desired)?;
    Ok(desired)
}

pub(super) fn settings_context(path: &Path, error: AppError) -> AppError {
    match error {
        AppError::SettingsConflict(message) => {
            AppError::SettingsConflict(format!("{}: {message}", path.display()))
        }
        // Structured variants carry their own path/meaning; re-wrapping them
        // as `Settings` would flatten the platform source chain and mask the
        // incomplete/conflict distinction this error model exists to keep.
        AppError::Io { .. }
        | AppError::Settings { .. }
        | AppError::OperationIncomplete(_)
        | AppError::Platform { .. } => error,
        error => AppError::Settings {
            path: path.to_path_buf(),
            message: error.to_string(),
        },
    }
}

pub(super) fn ensure_distinct_targets(targets: &[TerminalSettingsTarget]) -> AppResult<()> {
    let mut paths = HashSet::new();
    for target in targets {
        if !paths.insert(path_key(&target.settings_path)) {
            return Err(AppError::InvalidConfiguration(format!(
                "duplicate Windows Terminal settings target {}",
                target.settings_path.display()
            )));
        }
    }
    Ok(())
}

pub(super) const fn channel_label(channel: TerminalChannel) -> &'static str {
    match channel {
        TerminalChannel::Stable => "stable",
        TerminalChannel::Preview => "preview",
        TerminalChannel::Canary => "canary",
        TerminalChannel::Unpackaged => "unpackaged",
        TerminalChannel::Portable => "portable",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::platform::windows::PlatformError;

    use super::*;

    #[test]
    fn duplicate_targets_differing_only_by_case_are_rejected() {
        let targets = vec![
            TerminalSettingsTarget {
                channel: TerminalChannel::Stable,
                settings_path: PathBuf::from(r"C:\Users\Me\settings.json"),
            },
            TerminalSettingsTarget {
                channel: TerminalChannel::Preview,
                settings_path: PathBuf::from(r"c:\users\me\SETTINGS.JSON"),
            },
        ];
        let error =
            ensure_distinct_targets(&targets).expect_err("case-only duplicates must be rejected");
        assert!(matches!(error, AppError::InvalidConfiguration(_)));
    }

    #[test]
    fn settings_context_prefixes_conflicts_but_preserves_structured_variants() {
        let path = Path::new(r"C:\Users\Me\settings.json");

        let conflict = settings_context(path, AppError::SettingsConflict("keys clash".to_owned()));
        assert!(
            matches!(&conflict, AppError::SettingsConflict(message) if message.starts_with(r"C:\Users\Me\settings.json: ")),
            "conflicts must keep their path prefix: {conflict:?}"
        );

        let platform =
            settings_context(path, AppError::platform(PlatformError::HookThreadPanicked));
        assert!(
            matches!(platform, AppError::Platform { .. }),
            "platform errors must keep their source chain: {platform:?}"
        );
        assert!(std::error::Error::source(&platform).is_some());

        let incomplete = settings_context(
            path,
            AppError::OperationIncomplete("half applied".to_owned()),
        );
        assert!(
            matches!(incomplete, AppError::OperationIncomplete(_)),
            "incomplete operations must not be flattened into Settings: {incomplete:?}"
        );
    }
}
