//! Safe Windows Terminal settings integration.
//!
//! Windows Terminal 1.21+ loads action definitions from a fragment, but ignores
//! fragment key chords. WinTerminalP therefore installs commands in the fragment
//! and losslessly merges only their synthetic bridge chords into each initialized
//! channel's root `keybindings` array.

mod discovery;
mod encoding;
mod fragment;
mod helpers;
mod jsonc;
mod manifest;
mod rollback;
mod shell;
mod targets;
mod transaction;
mod types;

use crate::{AppError, AppResult};

pub use discovery::discover_targets;
use fragment::{
    execute_uninstall_fragment, install_fragment, plan_uninstall_fragment, prepare_fragment,
};
use helpers::desired_keybindings;
use manifest::{
    IntegrationManifest, LoadedManifest, load_manifest, remove_manifest_if_unchanged, save_manifest,
};
use rollback::rollback_install;
use targets::{
    TargetPreflight, diagnose_targets, execute_uninstall_target, install_target, plan_target,
    plan_uninstall_target, preflight_target, validated_targets,
};
pub use types::*;

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
            Err(rollback_error) => Err(AppError::OperationIncomplete(format!(
                "installation failed: {install_error}; rollback was incomplete: {rollback_error}; raw backups were retained in {}",
                config.state_dir.join("backups").display()
            ))),
        },
    }
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

    let target_reports = diagnose_targets(targets, &desired, &mut issues);

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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;

    use serde_json::{Value, json};

    use crate::TerminalChannel;
    use crate::keymap::managed_bindings;

    use super::fragment::desired_fragment;
    use super::targets::BEFORE_TARGET_WRITE_HOOK;
    use super::transaction::sha256_hex;
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
}
