//! Safe Windows Terminal settings integration.
//!
//! Windows Terminal 1.21+ loads action definitions from a fragment, but ignores
//! fragment key chords. WinTerminalP therefore installs commands in the fragment
//! and losslessly merges only their synthetic bridge chords into each initialized
//! channel's root `keybindings` array.

mod discovery;
mod jsonc;
mod manifest;
mod shell;
mod transaction;
mod types;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::keymap::managed_bindings;
use crate::{AppError, AppResult, TerminalChannel};

pub use discovery::discover_targets;
use jsonc::{
    DesiredKeybinding, analyze_keybindings, desired_keybinding, merge_keybindings,
    remove_managed_keybindings, validate_desired_bindings,
};
use manifest::{
    FragmentManifest, IntegrationManifest, LoadedManifest, ManagedKeybindingManifest,
    TargetManifest, load_manifest, remove_manifest_if_unchanged, save_manifest,
};
use transaction::{
    FileSnapshot, atomic_replace, create_backup, read_optional_snapshot, read_snapshot,
    remove_if_hash, sha256_hex,
};
pub use types::*;

struct FragmentPreparation {
    desired_value: Value,
    desired_bytes: Vec<u8>,
    semantically_equal: bool,
    current: Option<FileSnapshot>,
    report: FragmentReport,
}

struct TargetPreparation {
    target: TerminalSettingsTarget,
    snapshot: FileSnapshot,
    edit: jsonc::SettingsEdit,
}

enum UndoOperation {
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

/// Produces a read-only installation plan. No directory or file is created.
pub fn plan(config: &IntegrationConfig) -> AppResult<PlanReport> {
    let desired = desired_keybindings()?;
    let loaded_manifest = load_manifest(&config.manifest_path())?;
    let fragment = prepare_fragment(config, &loaded_manifest.manifest)?;
    let targets = validated_targets(config)?;
    let mut issues = Vec::new();
    if targets.is_empty() {
        issues.push(
            "no initialized Windows Terminal Stable, Preview, Canary, or Unpackaged settings file was found"
                .to_owned(),
        );
    }
    let target_plans = targets
        .iter()
        .map(|target| plan_target(target, &desired, &loaded_manifest.manifest))
        .collect::<Vec<_>>();
    let fragment_conflict = fragment.report.status == ChangeStatus::Conflict;
    let target_conflict = target_plans
        .iter()
        .any(|target| !target.conflicts.is_empty());
    Ok(PlanReport {
        schema_version: INTEGRATION_SCHEMA_VERSION,
        can_install: !targets.is_empty() && !fragment_conflict && !target_conflict,
        fragment: fragment.report,
        targets: target_plans,
        shell_integration: shell::plan(config),
        manifest_path: config.manifest_path(),
        issues,
    })
}

/// Installs the action fragment and bridge keybindings using compare-and-swap writes.
///
/// A failure on one channel (unreadable or invalid settings, keybinding
/// conflicts) is recorded on that target's report entry and never blocks the
/// remaining channels; install only fails when the fragment step fails or
/// every target fails.
pub fn install(config: &IntegrationConfig) -> AppResult<InstallReport> {
    let desired = desired_keybindings()?;
    let targets = validated_targets(config)?;
    if targets.is_empty() {
        return Err(AppError::TerminalNotInstalled);
    }
    let loaded_manifest = load_manifest(&config.manifest_path())?;
    let fragment = prepare_fragment(config, &loaded_manifest.manifest)?;
    if fragment.report.status == ChangeStatus::Conflict {
        return Err(AppError::SettingsConflict(
            fragment
                .report
                .message
                .clone()
                .unwrap_or_else(|| "unmanaged action fragment already exists".to_owned()),
        ));
    }

    let preflight = targets
        .into_iter()
        .map(|target| preflight_target(&target, &desired))
        .collect::<Vec<_>>();
    let failed_reports = preflight
        .iter()
        .filter_map(|entry| match entry {
            TargetPreflight::Failed(report) => Some(report),
            TargetPreflight::Ready(_) => None,
        })
        .collect::<Vec<_>>();
    if failed_reports.len() == preflight.len() && !preflight.is_empty() {
        return Err(AppError::SettingsConflict(
            failed_reports
                .iter()
                .filter_map(|report| report.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let mut applied = Vec::new();
    let operation = (|| {
        let mut next_manifest = loaded_manifest.manifest.clone();
        let installed_fragment_report =
            install_fragment(config, fragment, &mut next_manifest, &mut applied)?;
        let mut target_reports = Vec::with_capacity(preflight.len());
        for entry in preflight {
            match entry {
                TargetPreflight::Ready(preparation) => target_reports.push(install_target(
                    config,
                    preparation,
                    &mut next_manifest,
                    &mut applied,
                )?),
                TargetPreflight::Failed(report) => target_reports.push(report),
            }
        }

        if next_manifest != loaded_manifest.manifest {
            save_manifest(
                &config.manifest_path(),
                loaded_manifest.sha256.as_deref(),
                &next_manifest,
            )?;
        }

        Ok(InstallReport {
            schema_version: INTEGRATION_SCHEMA_VERSION,
            fragment: installed_fragment_report,
            targets: target_reports,
            shell_integration: Vec::new(),
            manifest_path: config.manifest_path(),
        })
    })();

    match operation {
        Ok(mut report) => {
            report.shell_integration = shell::install(config);
            Ok(report)
        }
        Err(install_error) => match rollback_install(&mut applied) {
            Ok(()) => Err(install_error),
            Err(rollback_error) => Err(AppError::SettingsConflict(format!(
                "installation failed: {install_error}; rollback was incomplete: {rollback_error}; raw backups were retained in {}",
                config.state_dir.join("backups").display()
            ))),
        },
    }
}

enum TargetPreflight {
    Ready(TargetPreparation),
    Failed(TargetInstallReport),
}

fn preflight_target(
    target: &TerminalSettingsTarget,
    desired: &[DesiredKeybinding],
) -> TargetPreflight {
    let failed = |status: ChangeStatus, before_sha256: Option<String>, message: String| {
        let after_sha256 = before_sha256.clone();
        TargetPreflight::Failed(TargetInstallReport {
            channel: target.channel,
            settings_path: target.settings_path.clone(),
            status,
            added_binding_count: 0,
            backup_path: None,
            before_sha256,
            after_sha256,
            message: Some(message),
        })
    };
    let snapshot = match read_snapshot(&target.settings_path) {
        Ok(snapshot) => snapshot,
        Err(error) => return failed(ChangeStatus::Skipped, None, error.to_string()),
    };
    let before_sha256 = Some(snapshot.sha256.clone());
    let edit = match merge_keybindings(&snapshot.bytes, desired) {
        Ok(edit) => edit,
        Err(error) => {
            let error = settings_context(&target.settings_path, error);
            return failed(ChangeStatus::Conflict, before_sha256, error.to_string());
        }
    };
    if !edit.conflicts.is_empty() {
        return failed(
            ChangeStatus::Conflict,
            before_sha256,
            format!(
                "{}: {}",
                target.settings_path.display(),
                edit.conflicts
                    .iter()
                    .map(|conflict| conflict.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
    }
    TargetPreflight::Ready(TargetPreparation {
        target: target.clone(),
        snapshot,
        edit,
    })
}

/// Removes only definitions that still semantically match the installation manifest.
///
/// Pre-flight validates every read-only decision before any file is mutated;
/// unexpected mid-execution failures are persisted to the manifest and reported
/// through `issues` instead of aborting halfway. `Err` is reserved for
/// pre-flight and manifest-persistence failures.
pub fn uninstall(config: &IntegrationConfig) -> AppResult<UninstallReport> {
    let loaded = load_manifest(&config.manifest_path())?;
    if loaded.sha256.is_none() {
        let shell_integration = shell::uninstall(config);
        return Ok(UninstallReport {
            schema_version: INTEGRATION_SCHEMA_VERSION,
            fragment_status: ChangeStatus::Missing,
            targets: Vec::new(),
            shell_integration,
            manifest_path: config.manifest_path(),
            manifest_retained: false,
            issues: Vec::new(),
        });
    }

    let fragment_plan = plan_uninstall_fragment(&loaded.manifest)?;
    let mut target_plans = Vec::with_capacity(loaded.manifest.targets.len());
    for target in &loaded.manifest.targets {
        target_plans.push(plan_uninstall_target(target)?);
    }

    let shell_integration = shell::uninstall(config);
    let mut retained_manifest = IntegrationManifest::default();
    let mut issues = Vec::new();

    let fragment_status = match execute_uninstall_fragment(
        config,
        &loaded.manifest,
        fragment_plan,
        &mut retained_manifest,
    ) {
        Ok(status) => status,
        Err(error) => {
            retained_manifest.fragment = loaded.manifest.fragment.clone();
            issues.push(format!("action fragment: {error}"));
            ChangeStatus::Skipped
        }
    };

    let mut target_reports = Vec::with_capacity(loaded.manifest.targets.len());
    for (target, plan) in loaded.manifest.targets.iter().zip(target_plans) {
        let executed = match plan {
            Some(plan) => execute_uninstall_target(config, target, plan),
            None => Ok((
                TargetUninstallReport {
                    channel: target.channel,
                    settings_path: target.settings_path.clone(),
                    status: ChangeStatus::Missing,
                    removed_binding_count: 0,
                    preserved_binding_count: 0,
                    backup_path: None,
                    message: None,
                },
                None,
            )),
        };
        match executed {
            Ok((report, retained)) => {
                if let Some(retained) = retained {
                    retained_manifest.targets.push(retained);
                }
                target_reports.push(report);
            }
            Err(error) => {
                issues.push(format!("{}: {error}", target.settings_path.display()));
                retained_manifest.targets.push(target.clone());
                target_reports.push(TargetUninstallReport {
                    channel: target.channel,
                    settings_path: target.settings_path.clone(),
                    status: ChangeStatus::Skipped,
                    removed_binding_count: 0,
                    preserved_binding_count: 0,
                    backup_path: None,
                    message: Some(error.to_string()),
                });
            }
        }
    }

    let manifest_retained =
        retained_manifest.fragment.is_some() || !retained_manifest.targets.is_empty();
    if manifest_retained {
        save_manifest(
            &config.manifest_path(),
            loaded.sha256.as_deref(),
            &retained_manifest,
        )?;
    } else {
        remove_manifest_if_unchanged(&config.manifest_path(), loaded.sha256.as_deref())?;
    }

    Ok(UninstallReport {
        schema_version: INTEGRATION_SCHEMA_VERSION,
        fragment_status,
        targets: target_reports,
        shell_integration,
        manifest_path: config.manifest_path(),
        manifest_retained,
        issues,
    })
}

/// Diagnoses fragments, initialized settings files, conflicts, and manifest integrity without writes.
pub fn doctor(config: &IntegrationConfig) -> AppResult<DoctorReport> {
    let desired = desired_keybindings()?;
    let (loaded_manifest, manifest_valid, manifest_issue) =
        match load_manifest(&config.manifest_path()) {
            Ok(loaded) => (loaded, true, None),
            Err(error) => (
                LoadedManifest {
                    manifest: IntegrationManifest::default(),
                    sha256: None,
                },
                false,
                Some(error.to_string()),
            ),
        };
    let fragment = prepare_fragment(config, &loaded_manifest.manifest)?;
    let mut targets = validated_targets(config)?;
    for recorded in &loaded_manifest.manifest.targets {
        if !targets
            .iter()
            .any(|target| path_key(&target.settings_path) == path_key(&recorded.settings_path))
        {
            targets.push(TerminalSettingsTarget {
                channel: recorded.channel,
                settings_path: recorded.settings_path.clone(),
            });
        }
    }

    let mut issues = Vec::new();
    if let Some(issue) = manifest_issue {
        issues.push(issue);
    }
    if fragment.report.status == ChangeStatus::Conflict {
        issues.push(
            fragment.report.message.clone().unwrap_or_else(|| {
                "action fragment conflicts with the managed fragment".to_owned()
            }),
        );
    }
    if matches!(
        fragment.report.status,
        ChangeStatus::Create | ChangeStatus::Update | ChangeStatus::Missing
    ) {
        issues.push("WinTerminalP action fragment is missing or requires an update".to_owned());
    }
    if targets.is_empty() {
        issues.push("no initialized Windows Terminal settings files were found".to_owned());
    }

    let mut target_reports = Vec::with_capacity(targets.len());
    for target in targets {
        match read_optional_snapshot(&target.settings_path) {
            Ok(Some(snapshot)) => match analyze_keybindings(&snapshot.bytes, &desired) {
                Ok(analysis) => {
                    if !analysis.conflicts.is_empty() {
                        issues.push(format!(
                            "{} has {} integration conflict(s)",
                            target.settings_path.display(),
                            analysis.conflicts.len()
                        ));
                    }
                    let warnings = analysis.warnings;
                    target_reports.push(DoctorTargetReport {
                        channel: target.channel,
                        settings_path: target.settings_path,
                        initialized: true,
                        readable: true,
                        valid_jsonc: true,
                        managed_binding_count: analysis.managed_binding_count,
                        conflicts: analysis.conflicts,
                        message: if warnings.is_empty() {
                            None
                        } else {
                            Some(warnings.join("; "))
                        },
                    });
                }
                Err(error) => {
                    issues.push(format!("{}: {error}", target.settings_path.display()));
                    target_reports.push(DoctorTargetReport {
                        channel: target.channel,
                        settings_path: target.settings_path,
                        initialized: true,
                        readable: true,
                        valid_jsonc: false,
                        managed_binding_count: 0,
                        conflicts: vec![IntegrationConflict {
                            kind: ConflictKind::InvalidSettingsShape,
                            action_id: None,
                            keys: None,
                            message: error.to_string(),
                        }],
                        message: Some(error.to_string()),
                    });
                }
            },
            Ok(None) => {
                let message = "settings file is no longer present".to_owned();
                issues.push(format!("{}: {message}", target.settings_path.display()));
                target_reports.push(DoctorTargetReport {
                    channel: target.channel,
                    settings_path: target.settings_path,
                    initialized: false,
                    readable: false,
                    valid_jsonc: false,
                    managed_binding_count: 0,
                    conflicts: Vec::new(),
                    message: Some(message),
                });
            }
            Err(error) => {
                issues.push(format!("{}: {error}", target.settings_path.display()));
                target_reports.push(DoctorTargetReport {
                    channel: target.channel,
                    settings_path: target.settings_path,
                    initialized: true,
                    readable: false,
                    valid_jsonc: false,
                    managed_binding_count: 0,
                    conflicts: Vec::new(),
                    message: Some(error.to_string()),
                });
            }
        }
    }

    Ok(DoctorReport {
        schema_version: INTEGRATION_SCHEMA_VERSION,
        healthy: manifest_valid && issues.is_empty(),
        fragment: fragment.report,
        targets: target_reports,
        shell_integration: shell::plan(config),
        manifest_path: config.manifest_path(),
        manifest_valid,
        issues,
    })
}

fn desired_keybindings() -> AppResult<Vec<DesiredKeybinding>> {
    let desired = managed_bindings()
        .iter()
        .copied()
        .map(|binding| desired_keybinding(binding.keybinding_definition_json()))
        .collect::<AppResult<Vec<_>>>()?;
    validate_desired_bindings(&desired)?;
    Ok(desired)
}

fn desired_fragment() -> AppResult<(Value, Vec<u8>)> {
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

fn prepare_fragment(
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

fn install_fragment(
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
                AppError::SettingsConflict("fragment disappeared during installation".to_owned())
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
                if !hash_matches {
                    if let Some(current_sha256) = current_sha256 {
                        record.installed_sha256 = current_sha256;
                    }
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

fn plan_target(
    target: &TerminalSettingsTarget,
    desired: &[DesiredKeybinding],
    manifest: &IntegrationManifest,
) -> TargetPlan {
    let old_managed_count = manifest
        .targets
        .iter()
        .find(|record| path_key(&record.settings_path) == path_key(&target.settings_path))
        .map_or(0, |record| record.managed_keybindings.len());
    match read_snapshot(&target.settings_path)
        .and_then(|snapshot| analyze_keybindings(&snapshot.bytes, desired))
    {
        Ok(analysis) => TargetPlan {
            channel: target.channel,
            settings_path: target.settings_path.clone(),
            status: if !analysis.conflicts.is_empty() {
                ChangeStatus::Conflict
            } else if analysis.bindings_to_add == 0 {
                ChangeStatus::Unchanged
            } else {
                ChangeStatus::Update
            },
            existing_binding_count: analysis.existing_binding_count,
            bindings_to_add: analysis.bindings_to_add,
            managed_binding_count: analysis.managed_binding_count.max(old_managed_count),
            conflicts: analysis.conflicts,
        },
        Err(error) => TargetPlan {
            channel: target.channel,
            settings_path: target.settings_path.clone(),
            status: ChangeStatus::Conflict,
            existing_binding_count: 0,
            bindings_to_add: 0,
            managed_binding_count: old_managed_count,
            conflicts: vec![IntegrationConflict {
                kind: ConflictKind::InvalidSettingsShape,
                action_id: None,
                keys: None,
                message: error.to_string(),
            }],
        },
    }
}

fn install_target(
    config: &IntegrationConfig,
    preparation: TargetPreparation,
    manifest: &mut IntegrationManifest,
    applied: &mut Vec<UndoOperation>,
) -> AppResult<TargetInstallReport> {
    let old_record = manifest
        .targets
        .iter()
        .find(|record| {
            path_key(&record.settings_path) == path_key(&preparation.target.settings_path)
        })
        .cloned();
    let before_sha256 = preparation.snapshot.sha256.clone();
    let mut backup_path = None;
    let (status, after_sha256, new_backup) = match preparation.edit.replacement.as_ref() {
        Some(replacement) => {
            let backup = create_backup(
                &config.state_dir,
                channel_label(preparation.target.channel),
                &preparation.target.settings_path,
                &preparation.snapshot,
            )?;
            backup_path = Some(backup.backup_path.clone());
            run_before_target_write_hook(&preparation.target.settings_path);
            let after_sha256 = apply_install_change(
                &preparation.target.settings_path,
                Some(&preparation.snapshot),
                replacement,
                applied,
            )?;
            (ChangeStatus::Update, after_sha256, Some(backup))
        }
        None => (
            ChangeStatus::Unchanged,
            preparation.snapshot.sha256.clone(),
            None,
        ),
    };

    let mut owned = old_record
        .as_ref()
        .map_or_else(Vec::new, |record| record.managed_keybindings.clone());
    owned.extend(preparation.edit.additions.iter().cloned());
    deduplicate_managed_records(&mut owned);
    manifest.targets.retain(|record| {
        path_key(&record.settings_path) != path_key(&preparation.target.settings_path)
    });
    if !owned.is_empty() {
        let backup = new_backup
            .or_else(|| old_record.as_ref().map(|record| record.backup.clone()))
            .ok_or_else(|| {
                AppError::InvalidConfiguration(
                    "managed settings record is missing its recovery backup".to_owned(),
                )
            })?;
        manifest.targets.push(TargetManifest {
            channel: preparation.target.channel,
            settings_path: preparation.target.settings_path.clone(),
            installed_sha256: after_sha256.clone(),
            backup,
            managed_keybindings: owned,
        });
    }

    Ok(TargetInstallReport {
        channel: preparation.target.channel,
        settings_path: preparation.target.settings_path,
        status,
        added_binding_count: preparation.edit.additions.len(),
        backup_path,
        before_sha256: Some(before_sha256),
        after_sha256: Some(after_sha256),
        message: None,
    })
}

fn apply_install_change(
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

fn rollback_install(applied: &mut Vec<UndoOperation>) -> AppResult<()> {
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
        Err(AppError::SettingsConflict(failures.join("; ")))
    }
}

#[cfg(test)]
type BeforeTargetWriteHook = std::cell::RefCell<Option<Box<dyn Fn(&Path)>>>;

#[cfg(test)]
thread_local! {
    static BEFORE_TARGET_WRITE_HOOK: BeforeTargetWriteHook = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_before_target_write_hook(path: &Path) {
    BEFORE_TARGET_WRITE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(path);
        }
    });
}

#[cfg(not(test))]
fn run_before_target_write_hook(_path: &Path) {}

enum FragmentUninstallPlan {
    Missing,
    Preserved,
    Remove(FileSnapshot),
}

fn plan_uninstall_fragment(manifest: &IntegrationManifest) -> AppResult<FragmentUninstallPlan> {
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

fn execute_uninstall_fragment(
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

struct TargetUninstallPlan {
    snapshot: FileSnapshot,
    edit: jsonc::RemovalEdit,
}

fn plan_uninstall_target(installed: &TargetManifest) -> AppResult<Option<TargetUninstallPlan>> {
    let Some(snapshot) = read_optional_snapshot(&installed.settings_path)? else {
        return Ok(None);
    };
    let edit = remove_managed_keybindings(&snapshot.bytes, &installed.managed_keybindings)
        .map_err(|error| settings_context(&installed.settings_path, error))?;
    Ok(Some(TargetUninstallPlan { snapshot, edit }))
}

fn execute_uninstall_target(
    config: &IntegrationConfig,
    installed: &TargetManifest,
    plan: TargetUninstallPlan,
) -> AppResult<(TargetUninstallReport, Option<TargetManifest>)> {
    let TargetUninstallPlan { snapshot, edit } = plan;
    let mut backup_path = None;
    let mut after_sha256 = snapshot.sha256.clone();
    if let Some(replacement) = edit.replacement.as_ref() {
        let backup = create_backup(
            &config.state_dir,
            &format!("{}-uninstall", channel_label(installed.channel)),
            &installed.settings_path,
            &snapshot,
        )?;
        backup_path = Some(backup.backup_path.clone());
        run_before_target_write_hook(&installed.settings_path);
        after_sha256 = atomic_replace(
            &installed.settings_path,
            Some(&snapshot.sha256),
            replacement,
        )?;
    }
    let retained = if edit.retained.is_empty() {
        None
    } else {
        Some(TargetManifest {
            channel: installed.channel,
            settings_path: installed.settings_path.clone(),
            installed_sha256: after_sha256,
            backup: installed.backup.clone(),
            managed_keybindings: edit.retained,
        })
    };
    let status = if edit.preserved_binding_count > 0 {
        ChangeStatus::Preserved
    } else if edit.removed_binding_count > 0 {
        ChangeStatus::Removed
    } else {
        ChangeStatus::Missing
    };
    Ok((
        TargetUninstallReport {
            channel: installed.channel,
            settings_path: installed.settings_path.clone(),
            status,
            removed_binding_count: edit.removed_binding_count,
            preserved_binding_count: edit.preserved_binding_count,
            backup_path,
            message: None,
        },
        retained,
    ))
}

fn validated_targets(config: &IntegrationConfig) -> AppResult<Vec<TerminalSettingsTarget>> {
    let targets = discover_targets(config);
    ensure_distinct_targets(&targets)?;
    Ok(targets)
}

fn ensure_distinct_targets(targets: &[TerminalSettingsTarget]) -> AppResult<()> {
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

fn parse_fragment_value(raw: &[u8]) -> AppResult<Value> {
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    serde_json::from_slice(raw).map_err(|error| {
        AppError::InvalidConfiguration(format!("action fragment is invalid JSON: {error}"))
    })
}

fn deduplicate_managed_records(records: &mut Vec<ManagedKeybindingManifest>) {
    let mut seen = HashSet::new();
    records.retain(|record| {
        seen.insert((record.canonical_id.clone(), record.canonical_chord.clone()))
    });
}

fn settings_context(path: &Path, error: AppError) -> AppError {
    match error {
        AppError::SettingsConflict(message) => {
            AppError::SettingsConflict(format!("{}: {message}", path.display()))
        }
        AppError::Io { .. } | AppError::Settings { .. } => error,
        error => AppError::Settings {
            path: path.to_path_buf(),
            message: error.to_string(),
        },
    }
}

const fn channel_label(channel: TerminalChannel) -> &'static str {
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
    use std::cell::Cell;
    use std::fs;
    use std::rc::Rc;

    use serde_json::Value;

    use super::*;

    fn channel_settings(root: &Path, package: &str, source: &[u8]) -> std::path::PathBuf {
        let path = root
            .join("Packages")
            .join(package)
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(path.parent().expect("fixture should have a parent"))
            .expect("fixture directory should be created");
        fs::write(&path, source).expect("fixture should be written");
        path
    }

    fn stable_settings(root: &Path, source: &[u8]) -> std::path::PathBuf {
        channel_settings(root, "Microsoft.WindowsTerminal_8wekyb3d8bbwe", source)
    }

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

    #[test]
    fn doctor_is_not_healthy_before_the_fragment_is_installed() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        stable_settings(temp.path(), b"{}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = doctor(&config).expect("doctor should be read-only and successful");

        assert_eq!(report.fragment.status, ChangeStatus::Create);
        assert!(!report.healthy);
        assert!(report.issues.iter().any(|issue| issue.contains("fragment")));
        assert!(!config.fragment_path().exists());
        assert!(!config.state_dir.exists());
    }

    #[test]
    fn tempdir_install_is_idempotent_and_records_raw_backup() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let original = b"{\r\n  // user settings\r\n  \"profiles\": { \"list\": [] },\r\n}\r\n";
        let settings_path = stable_settings(temp.path(), original);
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let first = install(&config).expect("first install should succeed");
        assert_eq!(first.targets.len(), 1);
        assert_eq!(
            first.targets[0].added_binding_count,
            managed_bindings().len()
        );
        let backup_path = first.targets[0]
            .backup_path
            .as_ref()
            .expect("changed settings should have a backup");
        assert_eq!(
            fs::read(backup_path).expect("backup should be readable"),
            original
        );
        assert!(config.fragment_path().is_file());
        assert!(config.manifest_path().is_file());

        let installed_once = fs::read(&settings_path).expect("settings should be readable");
        let second = install(&config).expect("second install should be idempotent");
        assert_eq!(second.targets[0].status, ChangeStatus::Unchanged);
        assert_eq!(second.targets[0].added_binding_count, 0);
        assert_eq!(
            fs::read(settings_path).expect("settings should remain readable"),
            installed_once
        );
    }

    #[test]
    fn uninstall_preserves_a_user_modified_managed_entry() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let settings_path = stable_settings(temp.path(), b"{\n  \"keybindings\": []\n}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        install(&config).expect("install should succeed");

        let source = fs::read_to_string(&settings_path).expect("settings should be readable");
        let mut value: Value =
            serde_json::from_str(&source).expect("installed settings should parse");
        let bindings = value["keybindings"]
            .as_array_mut()
            .expect("keybindings should be an array");
        bindings[0]["keys"] = Value::String("ctrl+alt+x".to_owned());
        bindings.push(json!({ "id": "User.Custom", "keys": "ctrl+q" }));
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&value).expect("fixture should serialize"),
        )
        .expect("modified settings should be written");

        let report = uninstall(&config).expect("uninstall should succeed safely");
        assert_eq!(report.targets[0].preserved_binding_count, 1);
        assert!(report.manifest_retained);
        let final_text = fs::read_to_string(&settings_path).expect("settings should be readable");
        assert!(final_text.contains("ctrl+alt+x"));
        assert!(final_text.contains("User.Custom"));
        assert!(!config.fragment_path().exists());
    }

    #[test]
    fn plan_blocks_an_unmanaged_fragment_without_writing_settings() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let settings_path = stable_settings(temp.path(), b"{}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        fs::create_dir_all(
            config
                .fragment_path()
                .parent()
                .expect("fragment should have a parent"),
        )
        .expect("fragment directory should be created");
        fs::write(config.fragment_path(), b"{\"actions\":[]}\n")
            .expect("foreign fragment should be written");
        let before = fs::read(&settings_path).expect("settings should be readable");

        let report = plan(&config).expect("plan should succeed");

        assert!(!report.can_install);
        assert_eq!(report.fragment.status, ChangeStatus::Conflict);
        assert_eq!(
            fs::read(settings_path).expect("settings should remain readable"),
            before
        );
        assert!(!config.manifest_path().exists());
    }

    #[test]
    fn second_target_cas_failure_rolls_back_first_target_and_fragment() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let first_original = b"{\r\n  // stable user bytes\r\n}\r\n";
        let second_original = b"{\n  // preview user bytes\n}\n";
        let first_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
            first_original,
        );
        let second_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            second_original,
        );
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let hook_calls = Rc::new(Cell::new(0));
        let hook_calls_for_callback = Rc::clone(&hook_calls);
        let second_path_for_callback = second_path.clone();
        BEFORE_TARGET_WRITE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path| {
                let call = hook_calls_for_callback.get() + 1;
                hook_calls_for_callback.set(call);
                if call == 2 {
                    assert_eq!(path, second_path_for_callback);
                    fs::write(path, b"{\"concurrentUserEdit\":true}\n")
                        .expect("concurrent fixture change should be written");
                }
            }));
        });

        let result = install(&config);
        BEFORE_TARGET_WRITE_HOOK.with(|hook| *hook.borrow_mut() = None);

        assert!(matches!(result, Err(AppError::SettingsConflict(_))));
        assert_eq!(hook_calls.get(), 2);
        assert_eq!(
            fs::read(&first_path).expect("first fixture should remain readable"),
            first_original
        );
        assert_eq!(
            fs::read(&second_path).expect("second fixture should remain readable"),
            b"{\"concurrentUserEdit\":true}\n"
        );
        assert!(!config.fragment_path().exists());
        assert!(!config.manifest_path().exists());
    }

    fn read_manifest_value(config: &IntegrationConfig) -> Value {
        let bytes = fs::read(config.manifest_path()).expect("manifest should be readable");
        serde_json::from_slice(&bytes).expect("manifest should be valid JSON")
    }

    #[test]
    fn install_retains_fragment_ownership_after_an_external_semantic_rewrite() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        stable_settings(temp.path(), b"{}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        install(&config).expect("first install should succeed");

        let fragment_path = config.fragment_path();
        let original = fs::read(&fragment_path).expect("fragment should be readable");
        let value: Value = serde_json::from_slice(&original).expect("fragment should parse");
        let rewritten = serde_json::to_vec(&value).expect("fragment should serialize");
        assert_ne!(rewritten, original, "rewrite must change the byte hash");
        fs::write(&fragment_path, &rewritten).expect("external rewrite should be written");

        let second = install(&config).expect("second install should succeed");
        assert_eq!(second.fragment.status, ChangeStatus::Unchanged);

        let manifest = read_manifest_value(&config);
        assert!(
            manifest["fragment"].is_object(),
            "fragment ownership record must survive an external semantic rewrite"
        );
        assert_eq!(
            manifest["fragment"]["installedSha256"],
            json!(sha256_hex(&rewritten))
        );

        let report = uninstall(&config).expect("uninstall should succeed");
        assert_eq!(report.fragment_status, ChangeStatus::Removed);
        assert!(!fragment_path.exists(), "uninstall should remove the file");
        assert!(!config.manifest_path().exists());
    }

    #[test]
    fn install_never_adopts_a_preexisting_unmanaged_fragment() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        stable_settings(temp.path(), b"{}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        let (_, desired_bytes) = desired_fragment().expect("fragment should be generated");
        fs::create_dir_all(
            config
                .fragment_path()
                .parent()
                .expect("fragment should have a parent"),
        )
        .expect("fragment directory should be created");
        fs::write(config.fragment_path(), &desired_bytes)
            .expect("pre-existing identical fragment should be written");

        let report = install(&config).expect("install should succeed");
        assert_eq!(report.fragment.status, ChangeStatus::Unchanged);

        let manifest = read_manifest_value(&config);
        assert!(
            manifest["fragment"].is_null(),
            "an independently created fragment must never be adopted"
        );

        let uninstall_report = uninstall(&config).expect("uninstall should succeed");
        assert_eq!(uninstall_report.fragment_status, ChangeStatus::Missing);
        assert!(
            config.fragment_path().exists(),
            "a never-managed fragment must be left in place"
        );
    }

    #[test]
    fn install_continues_when_one_target_settings_are_invalid() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let stable_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
            b"{\r\n  // stable user bytes\r\n}\r\n",
        );
        let preview_original = b"{ this is not valid jsonc\n";
        let preview_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            preview_original,
        );
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = install(&config).expect("one broken target must not abort the install");

        assert_eq!(report.targets.len(), 2);
        let stable = report
            .targets
            .iter()
            .find(|target| target.channel == TerminalChannel::Stable)
            .expect("stable target should be reported");
        assert_eq!(stable.status, ChangeStatus::Update);
        assert_eq!(stable.added_binding_count, managed_bindings().len());
        assert!(stable.message.is_none());
        assert!(stable.before_sha256.is_some());
        let preview = report
            .targets
            .iter()
            .find(|target| target.channel == TerminalChannel::Preview)
            .expect("preview target should be reported");
        assert_eq!(preview.status, ChangeStatus::Conflict);
        assert!(preview.message.is_some());
        assert_eq!(preview.added_binding_count, 0);

        let stable_text = fs::read_to_string(&stable_path).expect("stable should be readable");
        assert!(stable_text.contains("User.WinTerminalP."));
        assert_eq!(
            fs::read(&preview_path).expect("preview should remain readable"),
            preview_original,
            "the failing target must not be modified"
        );
        assert!(config.fragment_path().is_file());
        let manifest = read_manifest_value(&config);
        assert_eq!(
            manifest["targets"]
                .as_array()
                .expect("targets should be an array")
                .len(),
            1,
            "only the healthy target should be recorded"
        );
    }

    #[test]
    fn install_fails_without_writes_when_every_target_fails() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let settings_path = stable_settings(temp.path(), b"{ this is not valid jsonc\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        let before = fs::read(&settings_path).expect("settings should be readable");

        let result = install(&config);

        assert!(matches!(result, Err(AppError::SettingsConflict(_))));
        assert_eq!(
            fs::read(settings_path).expect("settings should remain readable"),
            before
        );
        assert!(!config.fragment_path().exists());
        assert!(!config.manifest_path().exists());
    }

    #[test]
    fn uninstall_persists_progress_when_a_target_fails_midway() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let stable_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
            b"{\r\n  // stable user bytes\r\n}\r\n",
        );
        let preview_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            b"{\n  // preview user bytes\n}\n",
        );
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        install(&config).expect("install should succeed");

        let failing_path_for_callback = preview_path.clone();
        BEFORE_TARGET_WRITE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path| {
                if path_key(path) == path_key(&failing_path_for_callback) {
                    let mut bytes = fs::read(path).expect("concurrent fixture should be readable");
                    bytes.extend_from_slice(b"\n// concurrent edit\n");
                    fs::write(path, bytes).expect("concurrent fixture change should be written");
                }
            }));
        });
        let result = uninstall(&config);
        BEFORE_TARGET_WRITE_HOOK.with(|hook| *hook.borrow_mut() = None);

        let report = result.expect("mid-way failure must be reported, not returned as Err");
        assert_eq!(report.targets.len(), 2);
        assert_eq!(report.fragment_status, ChangeStatus::Removed);
        assert!(!config.fragment_path().exists());
        assert!(report.manifest_retained);
        assert!(
            !report.issues.is_empty(),
            "the failed step must be described in issues"
        );

        let stable_report = report
            .targets
            .iter()
            .find(|target| target.channel == TerminalChannel::Stable)
            .expect("stable target should be reported");
        assert_eq!(stable_report.status, ChangeStatus::Removed);
        let preview_report = report
            .targets
            .iter()
            .find(|target| target.channel == TerminalChannel::Preview)
            .expect("preview target should be reported");
        assert_eq!(preview_report.status, ChangeStatus::Skipped);
        assert!(preview_report.message.is_some());

        let stable_text = fs::read_to_string(&stable_path).expect("stable should be readable");
        assert!(
            !stable_text.contains("User.WinTerminalP."),
            "the earlier removal must be persisted"
        );
        let preview_text = fs::read_to_string(&preview_path).expect("preview should be readable");
        assert!(
            preview_text.contains("User.WinTerminalP."),
            "the failed target must keep its bindings"
        );

        let manifest = read_manifest_value(&config);
        assert!(
            manifest["fragment"].is_null(),
            "fragment removal must persist"
        );
        let retained = manifest["targets"]
            .as_array()
            .expect("targets should be an array");
        assert_eq!(retained.len(), 1, "only the failed target must be retained");
        assert_eq!(
            path_key(Path::new(
                retained[0]["settingsPath"]
                    .as_str()
                    .expect("settingsPath should be a string")
            )),
            path_key(&preview_path)
        );
    }

    #[test]
    fn uninstall_pre_flight_failure_mutates_nothing() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let stable_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
            b"{\r\n  // stable user bytes\r\n}\r\n",
        );
        let preview_path = channel_settings(
            temp.path(),
            "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            b"{\n  // preview user bytes\n}\n",
        );
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        install(&config).expect("install should succeed");
        let manifest_before =
            fs::read(config.manifest_path()).expect("manifest should be readable");
        fs::write(&preview_path, b"{ this is not valid jsonc\n")
            .expect("corrupt fixture should be written");

        let result = uninstall(&config);

        assert!(
            matches!(
                result,
                Err(AppError::Settings { .. } | AppError::SettingsConflict(_))
            ),
            "a predictable pre-flight error must abort the uninstall"
        );
        assert!(
            config.fragment_path().is_file(),
            "the fragment must survive an aborted pre-flight"
        );
        assert_eq!(
            fs::read(config.manifest_path()).expect("manifest should be readable"),
            manifest_before,
            "the manifest must be untouched after an aborted pre-flight"
        );
        let stable_text = fs::read_to_string(&stable_path).expect("stable should be readable");
        assert!(
            stable_text.contains("User.WinTerminalP."),
            "no target may be mutated before pre-flight succeeds"
        );
    }

    #[test]
    fn path_key_is_case_and_separator_insensitive() {
        assert_eq!(
            path_key(Path::new(r"C:\Users\Me\AppData\Local")),
            path_key(Path::new(r"c:\users\me\appdata\LOCAL"))
        );
        assert_eq!(
            path_key(Path::new("C:/Users/Me/AppData")),
            path_key(Path::new(r"C:\Users\Me\AppData"))
        );
        assert_eq!(
            path_key(Path::new(r"\\?\C:\Temp\file.json")),
            path_key(Path::new(r"c:\temp\FILE.json"))
        );
        assert_ne!(
            path_key(Path::new(r"C:\Temp\a.json")),
            path_key(Path::new(r"C:\Temp\b.json"))
        );
    }

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
    fn ownership_survives_a_localappdata_casing_change() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        stable_settings(temp.path(), b"{}\n");
        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        install(&config).expect("first install should succeed");

        let manifest_path = config.manifest_path();
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest should be readable"))
                .expect("manifest should parse");
        manifest["fragment"]["path"] = Value::String(
            manifest["fragment"]["path"]
                .as_str()
                .unwrap()
                .to_uppercase(),
        );
        for record in manifest["targets"].as_array_mut().expect("targets array") {
            record["settingsPath"] =
                Value::String(record["settingsPath"].as_str().unwrap().to_uppercase());
        }
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest should serialize"),
        )
        .expect("re-cased manifest should be written");

        let second = install(&config).expect("install after casing change should succeed");
        assert_eq!(second.fragment.status, ChangeStatus::Unchanged);

        let saved = read_manifest_value(&config);
        assert!(
            saved["fragment"].is_object(),
            "fragment ownership must survive a casing change"
        );
        assert_eq!(
            saved["targets"]
                .as_array()
                .expect("targets should be an array")
                .len(),
            1,
            "target ownership must survive a casing change"
        );

        let report = uninstall(&config).expect("uninstall should succeed");
        assert_eq!(report.fragment_status, ChangeStatus::Removed);
        assert_eq!(report.targets[0].status, ChangeStatus::Removed);
        assert!(!config.fragment_path().exists());
        assert!(!config.manifest_path().exists());
    }

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
}
