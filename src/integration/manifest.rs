use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AppError, AppResult, TerminalChannel};

use super::INTEGRATION_SCHEMA_VERSION;
use super::transaction::{atomic_replace, read_optional_snapshot};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IntegrationManifest {
    pub schema_version: u32,
    pub fragment: Option<FragmentManifest>,
    pub targets: Vec<TargetManifest>,
}

impl Default for IntegrationManifest {
    fn default() -> Self {
        Self {
            schema_version: INTEGRATION_SCHEMA_VERSION,
            fragment: None,
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FragmentManifest {
    pub path: PathBuf,
    pub installed_sha256: String,
    pub backup: Option<BackupManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TargetManifest {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
    pub installed_sha256: String,
    pub backup: BackupManifest,
    pub managed_keybindings: Vec<ManagedKeybindingManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupManifest {
    pub source_path: PathBuf,
    pub backup_path: PathBuf,
    pub sha256: String,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ManagedKeybindingManifest {
    pub canonical_id: String,
    pub canonical_chord: String,
    pub definition: Value,
}

#[derive(Debug)]
pub(crate) struct LoadedManifest {
    pub manifest: IntegrationManifest,
    pub sha256: Option<String>,
}

pub(crate) fn load_manifest(path: &Path) -> AppResult<LoadedManifest> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(LoadedManifest {
            manifest: IntegrationManifest::default(),
            sha256: None,
        });
    };
    if let Some(declared) = newer_schema_version(&snapshot.bytes) {
        return Err(AppError::Settings {
            path: path.to_path_buf(),
            message: format!(
                "manifest.json was written by a newer version of winter \
                 (schema {declared} > {}); upgrade winter to manage this installation",
                INTEGRATION_SCHEMA_VERSION
            ),
        });
    }
    let manifest: IntegrationManifest =
        serde_json::from_slice(&snapshot.bytes).map_err(|error| AppError::Settings {
            path: path.to_path_buf(),
            message: format!("integration manifest is invalid: {error}"),
        })?;
    if manifest.schema_version != INTEGRATION_SCHEMA_VERSION {
        return Err(AppError::Settings {
            path: path.to_path_buf(),
            message: format!(
                "unsupported manifest schema {}; expected {}",
                manifest.schema_version, INTEGRATION_SCHEMA_VERSION
            ),
        });
    }
    Ok(LoadedManifest {
        manifest,
        sha256: Some(snapshot.sha256),
    })
}

/// Returns the declared `schemaVersion` when it is newer than this build supports.
///
/// Probed before the strict `deny_unknown_fields` parse so a manifest written by a newer
/// winter yields an upgrade message instead of an opaque unknown-field error.
fn newer_schema_version(bytes: &[u8]) -> Option<u64> {
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let declared = root.get("schemaVersion")?.as_u64()?;
    (declared > u64::from(INTEGRATION_SCHEMA_VERSION)).then_some(declared)
}

pub(crate) fn save_manifest(
    path: &Path,
    expected_sha256: Option<&str>,
    manifest: &IntegrationManifest,
) -> AppResult<String> {
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        AppError::InvalidConfiguration(format!("serialize integration manifest: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_replace(path, expected_sha256, &bytes)
}

/// Deletes the manifest only when its current content still hashes to `expected_sha256`.
///
/// Verifies the content hash immediately before removal with no intervening fallible
/// operations; a concurrent swap between verification and removal is a known residual race,
/// because Windows provides no atomic compare-and-delete. Returns `Ok(false)` without touching
/// the file when no expected hash was recorded or the file is already gone, and reports a hash
/// mismatch as a `SettingsConflict` error that retains the file.
pub(crate) fn remove_manifest_if_unchanged(
    path: &Path,
    expected_sha256: Option<&str>,
) -> AppResult<bool> {
    let Some(expected_sha256) = expected_sha256 else {
        return Ok(false);
    };
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(false);
    };
    if snapshot.sha256 != expected_sha256 {
        // A hash mismatch means another actor edited the manifest between load
        // and removal: the same concurrent-user-edit situation as the
        // compare-and-swap check in `transaction.rs`, so this stays a
        // user-resolvable `SettingsConflict` (the file is retained for
        // inspection) instead of `OperationIncomplete`.
        return Err(AppError::SettingsConflict(format!(
            "{} changed during uninstall; it was retained",
            path.display()
        )));
    }
    fs::remove_file(path)
        .map_err(|error| AppError::io("remove integration manifest", path, error))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(temp: &tempfile::TempDir, contents: &[u8]) -> PathBuf {
        let path = temp.path().join("manifest.json");
        fs::write(&path, contents).expect("fixture should be written");
        path
    }

    #[test]
    fn future_schema_version_reports_upgrade_requirement() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = write_fixture(
            &temp,
            br#"{"schemaVersion":999,"fragment":null,"targets":[],"addedByNewerWinter":true}"#,
        );

        let error = load_manifest(&path).expect_err("future schema must be rejected");

        let message = error.to_string();
        assert!(
            message.contains("newer version of winter"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains(&format!("schema 999 > {}", INTEGRATION_SCHEMA_VERSION)),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("upgrade winter"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn past_schema_version_reports_unsupported_schema() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = write_fixture(
            &temp,
            br#"{"schemaVersion":0,"fragment":null,"targets":[]}"#,
        );

        let error = load_manifest(&path).expect_err("past schema must be rejected");

        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "unsupported manifest schema 0; expected {}",
                INTEGRATION_SCHEMA_VERSION
            )),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn current_schema_manifest_round_trips() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = write_fixture(
            &temp,
            br#"{"schemaVersion":1,"fragment":null,"targets":[]}"#,
        );

        let loaded = load_manifest(&path).expect("current schema must load");

        assert_eq!(loaded.manifest, IntegrationManifest::default());
        assert!(loaded.sha256.is_some());
    }

    #[test]
    fn missing_manifest_loads_the_default() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("manifest.json");

        let loaded = load_manifest(&path).expect("missing manifest must load");

        assert_eq!(loaded.manifest, IntegrationManifest::default());
        assert!(loaded.sha256.is_none());
    }

    #[test]
    fn remove_manifest_deletes_only_matching_content() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = write_fixture(
            &temp,
            br#"{"schemaVersion":1,"fragment":null,"targets":[]}"#,
        );
        let loaded = load_manifest(&path).expect("fixture should load");
        let sha256 = loaded.sha256.expect("fixture should have a hash");

        assert!(!remove_manifest_if_unchanged(&path, None).expect("no hash is a no-op"));
        let conflict = remove_manifest_if_unchanged(&path, Some("0"))
            .expect_err("stale hash must be rejected");
        assert!(matches!(conflict, AppError::SettingsConflict(_)));
        assert!(path.exists());

        assert!(
            remove_manifest_if_unchanged(&path, Some(&sha256))
                .expect("matching hash should delete")
        );
        assert!(!path.exists());
        assert!(
            !remove_manifest_if_unchanged(&path, Some(&sha256)).expect("missing file is a no-op")
        );
    }
}
