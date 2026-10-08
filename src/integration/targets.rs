//! Per-target settings lifecycle: preflight, planning, install, uninstall, and
//! diagnostics for each initialized Windows Terminal channel.

use std::collections::HashSet;
use std::path::Path;

use crate::{AppError, AppResult};

use super::discovery::discover_targets;
use super::helpers::{channel_label, ensure_distinct_targets, settings_context};
use super::jsonc::{
    self, DesiredKeybinding, analyze_keybindings, merge_keybindings, remove_managed_keybindings,
};
use super::manifest::{IntegrationManifest, ManagedKeybindingManifest, TargetManifest};
use super::rollback::{UndoOperation, apply_install_change};
use super::transaction::{
    FileSnapshot, atomic_replace, create_backup, read_optional_snapshot, read_snapshot,
};
use super::types::{
    ChangeStatus, ConflictKind, DoctorTargetReport, IntegrationConfig, IntegrationConflict,
    TargetInstallReport, TargetPlan, TargetUninstallReport, TerminalSettingsTarget, path_key,
};

pub(super) struct TargetPreparation {
    target: TerminalSettingsTarget,
    snapshot: FileSnapshot,
    edit: jsonc::SettingsEdit,
}

pub(super) enum TargetPreflight {
    Ready(TargetPreparation),
    Failed(TargetInstallReport),
}

pub(super) fn preflight_target(
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

pub(super) fn plan_target(
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

pub(super) fn install_target(
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

pub(super) struct TargetUninstallPlan {
    snapshot: FileSnapshot,
    edit: jsonc::RemovalEdit,
}

pub(super) fn plan_uninstall_target(
    installed: &TargetManifest,
) -> AppResult<Option<TargetUninstallPlan>> {
    let Some(snapshot) = read_optional_snapshot(&installed.settings_path)? else {
        return Ok(None);
    };
    let edit = remove_managed_keybindings(&snapshot.bytes, &installed.managed_keybindings)
        .map_err(|error| settings_context(&installed.settings_path, error))?;
    Ok(Some(TargetUninstallPlan { snapshot, edit }))
}

pub(super) fn execute_uninstall_target(
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

pub(super) fn diagnose_targets(
    targets: Vec<TerminalSettingsTarget>,
    desired: &[DesiredKeybinding],
    issues: &mut Vec<String>,
) -> Vec<DoctorTargetReport> {
    let mut target_reports = Vec::with_capacity(targets.len());
    for target in targets {
        match read_optional_snapshot(&target.settings_path) {
            Ok(Some(snapshot)) => match analyze_keybindings(&snapshot.bytes, desired) {
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
    target_reports
}

pub(super) fn validated_targets(
    config: &IntegrationConfig,
) -> AppResult<Vec<TerminalSettingsTarget>> {
    let targets = discover_targets(config);
    ensure_distinct_targets(&targets)?;
    Ok(targets)
}

fn deduplicate_managed_records(records: &mut Vec<ManagedKeybindingManifest>) {
    let mut seen = HashSet::new();
    records.retain(|record| {
        seen.insert((record.canonical_id.clone(), record.canonical_chord.clone()))
    });
}

#[cfg(test)]
type BeforeTargetWriteHook = std::cell::RefCell<Option<Box<dyn Fn(&Path)>>>;

#[cfg(test)]
thread_local! {
    pub(super) static BEFORE_TARGET_WRITE_HOOK: BeforeTargetWriteHook = std::cell::RefCell::new(None);
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
