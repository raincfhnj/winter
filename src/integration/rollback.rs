//! Compare-and-swap write tracking and rollback for interrupted installs.

use std::path::{Path, PathBuf};

use crate::{AppError, AppResult};

use super::transaction::{
    FileSnapshot, atomic_replace, read_optional_snapshot, remove_if_hash, sha256_hex,
};

pub(super) enum UndoOperation {
    Created {
        path: PathBuf,
        installed_sha256: String,
    },
    Replaced {
        path: PathBuf,
        installed_sha256: String,
        original: Vec<u8>,
    },
}

pub(super) fn apply_install_change(
    path: &Path,
    before: Option<&FileSnapshot>,
    replacement: &[u8],
    applied: &mut Vec<UndoOperation>,
) -> AppResult<String> {
    let expected_sha256 = before.map(|snapshot| snapshot.sha256.as_str());
    let replacement_sha256 = sha256_hex(replacement);
    match atomic_replace(path, expected_sha256, replacement) {
        Ok(installed_sha256) => {
            applied.push(undo_operation(path, before, installed_sha256.clone()));
            Ok(installed_sha256)
        }
        Err(error) => {
            let applied_state = match read_optional_snapshot(path) {
                Ok(None) => false,
                Ok(Some(snapshot)) => snapshot.sha256 == replacement_sha256,
                Err(_) => true,
            };
            if applied_state {
                applied.push(undo_operation(path, before, replacement_sha256));
            }
            Err(error)
        }
    }
}

fn undo_operation(
    path: &Path,
    before: Option<&FileSnapshot>,
    installed_sha256: String,
) -> UndoOperation {
    match before {
        Some(snapshot) => UndoOperation::Replaced {
            path: path.to_path_buf(),
            installed_sha256,
            original: snapshot.bytes.clone(),
        },
        None => UndoOperation::Created {
            path: path.to_path_buf(),
            installed_sha256,
        },
    }
}

pub(super) fn rollback_install(applied: &mut Vec<UndoOperation>) -> AppResult<()> {
    let mut failures = Vec::new();
    while let Some(operation) = applied.pop() {
        let result = match operation {
            UndoOperation::Created {
                path,
                installed_sha256,
            } => match remove_if_hash(&path, &installed_sha256) {
                Ok(true) => Ok(()),
                Ok(false) if !path.exists() => Ok(()),
                Ok(false) => Err(AppError::SettingsConflict(format!(
                    "{} changed after WinTerminalP created it; user bytes were preserved",
                    path.display()
                ))),
                Err(error) => Err(error),
            },
            UndoOperation::Replaced {
                path,
                installed_sha256,
                original,
            } => atomic_replace(&path, Some(&installed_sha256), &original).map(|_| ()),
        };
        if let Err(error) = result {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::OperationIncomplete(failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn undo_registration_is_conservative_when_the_state_cannot_be_read() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("unreadable-as-file");
        fs::create_dir(&path).expect("directory fixture should be created");
        let mut applied = Vec::new();

        let result = apply_install_change(&path, None, b"{}", &mut applied);

        assert!(result.is_err());
        assert_eq!(
            applied.len(),
            1,
            "an unknown applied state must still register an undo operation"
        );
    }

    #[test]
    fn rollback_failures_are_reported_as_an_incomplete_operation_not_a_conflict() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("fragment.json");
        fs::write(&path, b"{\"value\":2}").expect("fixture should be written");
        let mut applied = vec![UndoOperation::Created {
            path: path.clone(),
            installed_sha256: "0".repeat(64),
        }];

        let error =
            rollback_install(&mut applied).expect_err("mismatched content must fail the rollback");

        assert!(
            matches!(error, AppError::OperationIncomplete(_)),
            "a rollback that cannot restore everything must classify as incomplete: {error:?}"
        );
        assert!(
            applied.is_empty(),
            "every undo attempt must be consumed even when it fails"
        );
        assert_eq!(
            fs::read(&path).expect("user bytes must be preserved"),
            b"{\"value\":2}"
        );
    }
}
