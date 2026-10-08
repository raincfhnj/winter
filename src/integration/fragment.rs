//! Action fragment lifecycle: generation, preparation, install, and uninstall.

use serde_json::{Value, json};

use crate::keymap::managed_bindings;
use crate::{AppError, AppResult};

use super::manifest::{FragmentManifest, IntegrationManifest};
use super::rollback::{UndoOperation, apply_install_change};
use super::transaction::{
    FileSnapshot, create_backup, read_optional_snapshot, remove_if_hash, sha256_hex,
};
use super::types::{
    ChangeStatus, FragmentReport, IntegrationConfig, MINIMUM_FRAGMENT_VERSION, path_key,
};

pub(super) struct FragmentPreparation {
    desired_value: Value,
    desired_bytes: Vec<u8>,
    semantically_equal: bool,
    current: Option<FileSnapshot>,
    pub(super) report: FragmentReport,
}

pub(super) fn desired_fragment() -> AppResult<(Value, Vec<u8>)> {
    let actions = managed_bindings()
        .iter()
        .copied()
        .map(|binding| binding.action_definition_json())
        .collect::<Vec<_>>();
    let value = json!({ "actions": actions });
    if contains_forbidden_keybinding_field(&value) {
        return Err(AppError::InvalidConfiguration(
            "action fragment generation attempted to include keys or keybindings".to_owned(),
        ));
    }
    let mut bytes = serde_json::to_vec_pretty(&value).map_err(|error| {
        AppError::InvalidConfiguration(format!("serialize Windows Terminal fragment: {error}"))
    })?;
    bytes.push(b'\n');
    Ok((value, bytes))
}

fn contains_forbidden_keybinding_field(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(key.as_str(), "keys" | "keybindings")
                || contains_forbidden_keybinding_field(value)
        }),
        Value::Array(values) => values.iter().any(contains_forbidden_keybinding_field),
        _ => false,
    }
}

pub(super) fn prepare_fragment(
    config: &IntegrationConfig,
    manifest: &IntegrationManifest,
) -> AppResult<FragmentPreparation> {
    let path = config.fragment_path();
    let (desired_value, desired_bytes) = desired_fragment()?;
    let desired_sha256 = sha256_hex(&desired_bytes);
    let current = read_optional_snapshot(&path)?;
    let owned = manifest
        .fragment
        .as_ref()
        .filter(|record| path_key(&record.path) == path_key(&path))
        .filter(|record| {
            current
                .as_ref()
                .is_some_and(|snapshot| snapshot.sha256 == record.installed_sha256)
        });
    let semantically_equal = current
        .as_ref()
        .and_then(|snapshot| parse_fragment_value(&snapshot.bytes).ok())
        .is_some_and(|value| value == desired_value);
    let (status, message) = match current.as_ref() {
        None => (ChangeStatus::Create, None),
        Some(_) if semantically_equal => (ChangeStatus::Unchanged, None),
        Some(_) if owned.is_some() => (ChangeStatus::Update, None),
        Some(_) => (
            ChangeStatus::Conflict,
            Some(format!(
                "{} already exists but is not the fragment recorded by WinTerminalP",
                path.display()
            )),
        ),
    };
    let report = FragmentReport {
        path,
        status,
        current_sha256: current.as_ref().map(|snapshot| snapshot.sha256.clone()),
        desired_sha256,
        minimum_terminal_version: MINIMUM_FRAGMENT_VERSION.to_owned(),
        message,
    };
    Ok(FragmentPreparation {
        desired_value,
        desired_bytes,
        semantically_equal,
        current,
        report,
    })
}

pub(super) fn install_fragment(
    config: &IntegrationConfig,
    preparation: FragmentPreparation,
    manifest: &mut IntegrationManifest,
    applied: &mut Vec<UndoOperation>,
) -> AppResult<FragmentReport> {
    let mut report = preparation.report;
    let old_record = manifest.fragment.clone();
    match report.status {
        ChangeStatus::Create => {
            let installed_sha256 =
                apply_install_change(&report.path, None, &preparation.desired_bytes, applied)?;
            manifest.fragment = Some(FragmentManifest {
                path: report.path.clone(),
                installed_sha256: installed_sha256.clone(),
                backup: None,
            });
            report.current_sha256 = Some(installed_sha256);
        }
        ChangeStatus::Update => {
            let current = preparation.current.as_ref().ok_or_else(|| {
                AppError::OperationIncomplete("fragment disappeared during installation".to_owned())
            })?;
            let backup = create_backup(&config.state_dir, "fragment", &report.path, current)?;
            let installed_sha256 = apply_install_change(
                &report.path,
                Some(current),
                &preparation.desired_bytes,
                applied,
            )?;
            manifest.fragment = Some(FragmentManifest {
                path: report.path.clone(),
                installed_sha256: installed_sha256.clone(),
                backup: Some(backup),
            });
            report.current_sha256 = Some(installed_sha256);
        }
        ChangeStatus::Unchanged => {
            let current_sha256 = preparation
                .current
                .as_ref()
                .map(|current| current.sha256.clone());
            manifest.fragment = old_record.and_then(|mut record| {
                if path_key(&record.path) != path_key(&report.path) {
                    return None;
                }
                let hash_matches =
                    current_sha256.as_deref() == Some(record.installed_sha256.as_str());
                if !hash_matches && !preparation.semantically_equal {
                    return None;
                }
                if !hash_matches && let Some(current_sha256) = current_sha256 {
                    record.installed_sha256 = current_sha256;
                }
                Some(record)
            });
        }
        ChangeStatus::Conflict => {
            return Err(AppError::SettingsConflict(
                report
                    .message
                    .clone()
                    .unwrap_or_else(|| "action fragment conflict".to_owned()),
            ));
        }
        _ => {
            return Err(AppError::InvalidConfiguration(
                "invalid fragment installation state".to_owned(),
            ));
        }
    }
    debug_assert!(!contains_forbidden_keybinding_field(
        &preparation.desired_value
    ));
    Ok(report)
}

fn parse_fragment_value(raw: &[u8]) -> AppResult<Value> {
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    serde_json::from_slice(raw).map_err(|error| {
        AppError::InvalidConfiguration(format!("action fragment is invalid JSON: {error}"))
    })
}

pub(super) enum FragmentUninstallPlan {
    Missing,
    Preserved,
    Remove(FileSnapshot),
}

pub(super) fn plan_uninstall_fragment(
    manifest: &IntegrationManifest,
) -> AppResult<FragmentUninstallPlan> {
    let Some(fragment) = manifest.fragment.as_ref() else {
        return Ok(FragmentUninstallPlan::Missing);
    };
    let Some(snapshot) = read_optional_snapshot(&fragment.path)? else {
        return Ok(FragmentUninstallPlan::Missing);
    };
    if snapshot.sha256 != fragment.installed_sha256 {
        return Ok(FragmentUninstallPlan::Preserved);
    }
    Ok(FragmentUninstallPlan::Remove(snapshot))
}

pub(super) fn execute_uninstall_fragment(
    config: &IntegrationConfig,
    installed: &IntegrationManifest,
    plan: FragmentUninstallPlan,
    retained: &mut IntegrationManifest,
) -> AppResult<ChangeStatus> {
    let Some(fragment) = installed.fragment.as_ref() else {
        return Ok(ChangeStatus::Missing);
    };
    match plan {
        FragmentUninstallPlan::Missing => Ok(ChangeStatus::Missing),
        FragmentUninstallPlan::Preserved => {
            retained.fragment = Some(fragment.clone());
            Ok(ChangeStatus::Preserved)
        }
        FragmentUninstallPlan::Remove(snapshot) => {
            create_backup(
                &config.state_dir,
                "fragment-uninstall",
                &fragment.path,
                &snapshot,
            )?;
            if remove_if_hash(&fragment.path, &fragment.installed_sha256)? {
                Ok(ChangeStatus::Removed)
            } else {
                retained.fragment = Some(fragment.clone());
                Ok(ChangeStatus::Preserved)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_fragment_has_actions_but_no_keybindings() {
        let (value, bytes) = desired_fragment().expect("fragment should be generated");

        assert_eq!(
            value["actions"]
                .as_array()
                .expect("actions should be an array")
                .len(),
            managed_bindings().len()
        );
        assert!(!contains_forbidden_keybinding_field(&value));
        let text = std::str::from_utf8(&bytes).expect("fragment should be UTF-8");
        assert!(!text.contains("\"keys\""));
        assert!(!text.contains("\"keybindings\""));
    }
}
