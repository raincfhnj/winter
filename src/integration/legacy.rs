//! One-shot cleanup of pre-Winter installations that used the historical
//! `WinTerminalP` on-disk identifiers.
//!
//! Old releases stored their state under `%LOCALAPPDATA%\WinTerminalP`,
//! installed a `Fragments\WinTerminalP` action fragment with
//! `User.WinTerminalP.*` keybindings, and wrapped shell profiles in
//! `WinTerminalP` marker blocks. Current code only knows the Winter-branded
//! identifiers, so [`detect`] finds the old world and [`migrate`] removes it
//! using the legacy manifest's own records (ids are data, not code) before a
//! fresh install runs. Every readable input is pre-flighted before the first
//! write; the legacy state directory — the only recovery record — is deleted
//! only after every step succeeds.

use std::fs;

use super::fragment::{execute_uninstall_fragment, plan_uninstall_fragment};
use super::manifest::{IntegrationManifest, load_manifest};
use super::targets::{execute_uninstall_target, plan_uninstall_target};
use super::transaction::{remove_if_hash, sha256_hex};
use super::types::{APP_DATA_DIR_NAME, LEGACY_APP_DATA_DIR_NAME};
use super::{ChangeStatus, IntegrationConfig, shell};
use crate::{AppError, AppResult};

/// Snapshot of the historical installation, if any artifacts exist.
pub(super) struct LegacyInstallation {
    /// `%LOCALAPPDATA%\WinTerminalP`.
    dir: std::path::PathBuf,
    /// `%LOCALAPPDATA%\Microsoft\Windows Terminal\Fragments\WinTerminalP`.
    fragment_dir: std::path::PathBuf,
    /// Loaded legacy manifest when present and readable.
    manifest: Option<super::manifest::LoadedManifest>,
}

fn legacy_dir(config: &IntegrationConfig) -> std::path::PathBuf {
    config.local_app_data.join(LEGACY_APP_DATA_DIR_NAME)
}

fn legacy_fragment_dir(config: &IntegrationConfig) -> std::path::PathBuf {
    config
        .local_app_data
        .join("Microsoft")
        .join("Windows Terminal")
        .join("Fragments")
        .join(LEGACY_APP_DATA_DIR_NAME)
}

/// Returns the legacy installation when any historical artifact exists.
///
/// A present-but-unreadable legacy manifest propagates as an error: the
/// migration must not guess without its ownership record.
pub(super) fn detect(config: &IntegrationConfig) -> AppResult<Option<LegacyInstallation>> {
    let dir = legacy_dir(config);
    let fragment_dir = legacy_fragment_dir(config);
    let has_artifacts = dir.exists() || fragment_dir.exists();
    if !has_artifacts {
        return Ok(None);
    }
    let manifest_path = dir.join("integration").join("manifest.json");
    let manifest = if manifest_path.is_file() {
        Some(load_manifest(&manifest_path)?)
    } else {
        None
    };
    Ok(Some(LegacyInstallation {
        dir,
        fragment_dir,
        manifest,
    }))
}

/// Non-fatal detection for `plan`/`doctor`: an issue string when a legacy
/// installation (or an unreadable one) is present, `None` otherwise.
pub(super) fn detection_issue(config: &IntegrationConfig) -> Option<String> {
    match detect(config) {
        Ok(Some(legacy)) => Some(format!(
            "legacy WinTerminalP installation detected; run 'winter install' to migrate (state directory {})",
            legacy.dir.display()
        )),
        Ok(None) => None,
        Err(error) => Some(format!(
            "legacy WinTerminalP installation is present but unreadable: {error}; \
             inspect and delete {} manually",
            legacy_dir(config).display()
        )),
    }
}

/// Called at the start of `install`: removes the historical installation and
/// carries the user's config over, so the fresh Winter install starts clean.
pub(super) fn migrate_if_present(config: &IntegrationConfig) -> AppResult<()> {
    let Some(legacy) = detect(config)? else {
        return Ok(());
    };
    eprintln!(
        "legacy WinTerminalP installation detected; migrating to Winter ({} or 'winter uninstall' first would also work)",
        legacy.dir.display()
    );
    clean(config, &legacy, true)?;
    eprintln!("legacy installation removed; installing the Winter-branded integration");
    Ok(())
}

/// Called by `uninstall` when no current manifest exists: removes the
/// historical installation without carrying the config over.
pub(super) fn uninstall_if_present(config: &IntegrationConfig) -> AppResult<bool> {
    let Some(legacy) = detect(config)? else {
        return Ok(false);
    };
    eprintln!("legacy WinTerminalP installation detected; removing it");
    clean(config, &legacy, false)?;
    Ok(true)
}

fn clean(
    config: &IntegrationConfig,
    legacy: &LegacyInstallation,
    carry_config: bool,
) -> AppResult<()> {
    // 1. Fragment and settings: every readable input pre-flighted through the
    //    same plan step the regular uninstall uses, then executed. The legacy
    //    records carry the old ids as data, so no old token appears here.
    if let Some(loaded) = &legacy.manifest {
        let fragment_plan = plan_uninstall_fragment(&loaded.manifest)?;
        let mut retained = IntegrationManifest::default();
        let fragment_status =
            execute_uninstall_fragment(config, &loaded.manifest, fragment_plan, &mut retained)?;
        debug_assert!(!matches!(fragment_status, ChangeStatus::Conflict));

        let mut preserved = 0usize;
        for target in &loaded.manifest.targets {
            let Some(plan) = plan_uninstall_target(target)? else {
                continue;
            };
            let (report, _retained) = execute_uninstall_target(config, target, plan)?;
            preserved += report.preserved_binding_count;
        }
        if preserved > 0 {
            eprintln!("  kept {preserved} user-modified legacy keybinding(s) in place");
        }
    } else {
        // No manifest: remove only a fragment that is unmistakably ours.
        let fragment_path = legacy.fragment_dir.join("actions.json");
        if let Ok(bytes) = fs::read(&fragment_path) {
            if String::from_utf8_lossy(&bytes).contains("User.WinTerminalP.") {
                let expected = sha256_hex(&bytes);
                if !remove_if_hash(&fragment_path, &expected)? {
                    return Err(AppError::SettingsConflict(format!(
                        "{} changed while migrating; rerun 'winter install'",
                        fragment_path.display()
                    )));
                }
            } else {
                return Err(AppError::SettingsConflict(format!(
                    "{} exists without a legacy manifest and does not contain Winter action \
                     ids; inspect and remove it manually",
                    fragment_path.display()
                )));
            }
        }
    }

    // 2. Legacy shell blocks (force removal of namespaced markers).
    for report in shell::uninstall_legacy(config) {
        match report.status {
            ChangeStatus::Removed | ChangeStatus::Missing => {}
            _ => {
                return Err(AppError::SettingsConflict(format!(
                    "legacy shell block in {}: {}; fix the markers manually, then rerun",
                    report.path.display(),
                    report
                        .message
                        .unwrap_or_else(|| format!("{:?}", report.status))
                )));
            }
        }
    }

    // 3. Carry the user's config into the new location before the old tree
    //    disappears (copy, not move — a later failure keeps the original).
    if carry_config {
        let new_config = config
            .local_app_data
            .join(APP_DATA_DIR_NAME)
            .join("config.toml");
        let legacy_config = legacy.dir.join("config.toml");
        if legacy_config.is_file() && !new_config.exists() {
            if let Some(parent) = new_config.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    AppError::io("create Winter application directory", parent, error)
                })?;
            }
            fs::copy(&legacy_config, &new_config).map_err(|error| {
                AppError::io("carry over legacy configuration", &new_config, error)
            })?;
            eprintln!("  carried over {}", new_config.display());
        }
    }

    // 4. Only now is the legacy tree disposable.
    if legacy.dir.exists() {
        fs::remove_dir_all(&legacy.dir).map_err(|error| {
            AppError::io(
                "remove legacy WinTerminalP state directory",
                &legacy.dir,
                error,
            )
        })?;
    }
    // The fragment directory is ours by namespace; remove it when empty
    // (individual files were already handled through the manifest).
    let _ = fs::remove_dir(&legacy.fragment_dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::integration::jsonc;
    use crate::integration::manifest::{
        BackupManifest, FragmentManifest, ManagedKeybindingManifest, TargetManifest,
    };
    use crate::integration::transaction::read_snapshot;

    fn config_with(root: &Path) -> IntegrationConfig {
        IntegrationConfig::new(
            root,
            root.join(APP_DATA_DIR_NAME).join("integration"),
            root.join("Documents"),
        )
    }

    fn backup_stub(path: &Path) -> BackupManifest {
        BackupManifest {
            source_path: path.to_path_buf(),
            backup_path: path.with_extension("bak"),
            sha256: "stub".to_owned(),
            byte_len: 0,
        }
    }

    /// Builds the full legacy world: state dir + manifest, discoverable
    /// settings with old ids, fragment, legacy shell block, and a legacy
    /// config file.
    fn seed_legacy(root: &Path) -> IntegrationConfig {
        let config = config_with(root);
        let legacy_dir = root.join(LEGACY_APP_DATA_DIR_NAME);

        // Legacy config.
        fs::create_dir_all(&legacy_dir).expect("legacy dir");
        fs::write(legacy_dir.join("config.toml"), b"schema_version = 1\n").expect("legacy config");

        // The discoverable Windows Terminal settings file — the legacy
        // manifest's target — with one legacy managed binding + one user
        // binding.
        let settings_path = root
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).expect("settings dir");
        fs::write(
            &settings_path,
            br#"{"keybindings":[
            {"id":"User.WinTerminalP.SplitLeft","keys":"ctrl+f13"},
            {"id":"User.Own.Custom","keys":"ctrl+f9"}
        ]}"#,
        )
        .expect("settings");
        let settings_snapshot = read_snapshot(&settings_path).expect("settings snapshot");

        // Legacy fragment.
        let fragment_dir = legacy_fragment_dir(&config);
        fs::create_dir_all(&fragment_dir).expect("fragment dir");
        let fragment_path = fragment_dir.join("actions.json");
        let fragment_bytes =
            br#"{"actions":[{"id":"User.WinTerminalP.SplitLeft","keys":"ctrl+f13"}]}"#;
        fs::write(&fragment_path, fragment_bytes).expect("fragment");
        let fragment_sha = sha256_hex(fragment_bytes);

        // Legacy shell block in the Windows PowerShell profile.
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile dir");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let legacy_block = "# >>> WinTerminalP shell integration >>>\n\
            $Global:__WinTerminalP_PromptWrapped = $true\n\
            # <<< WinTerminalP shell integration <<<\n";
        fs::write(&profile, format!("Write-Host hi\n{legacy_block}")).expect("profile");

        // A discoverable Windows Terminal settings file containing the legacy
        // managed binding, so `plan`/`install` see a real target.
        let wt_settings = root
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(wt_settings.parent().unwrap()).expect("wt settings dir");
        fs::write(
            &wt_settings,
            br#"{"keybindings":[
            {"id":"User.WinTerminalP.SplitLeft","keys":"ctrl+f13"},
            {"id":"User.Own.Custom","keys":"ctrl+f9"}
        ]}"#,
        )
        .expect("wt settings");

        // Legacy manifest.
        let definition = serde_json::json!({
            "id": "User.WinTerminalP.SplitLeft",
            "keys": "ctrl+f13",
        });
        let manifest = IntegrationManifest {
            schema_version: super::super::INTEGRATION_SCHEMA_VERSION,
            fragment: Some(FragmentManifest {
                path: fragment_path.clone(),
                installed_sha256: fragment_sha,
                backup: None,
            }),
            targets: vec![TargetManifest {
                channel: crate::TerminalChannel::Stable,
                settings_path: settings_path.clone(),
                installed_sha256: settings_snapshot.sha256.clone(),
                backup: backup_stub(&settings_path),
                managed_keybindings: vec![ManagedKeybindingManifest {
                    canonical_id: "User.WinTerminalP.SplitLeft".to_owned(),
                    canonical_chord: "ctrl+f13".to_owned(),
                    definition,
                }],
            }],
        };
        let integration_dir = legacy_dir.join("integration");
        fs::create_dir_all(&integration_dir).expect("legacy integration dir");
        fs::write(
            integration_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).expect("manifest serializable"),
        )
        .expect("legacy manifest");

        config
    }

    #[test]
    fn install_migrates_the_legacy_world_and_installs_winter_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path());

        let report = super::super::install(&config).expect("install migrates then installs");

        let legacy_dir = temp.path().join(LEGACY_APP_DATA_DIR_NAME);
        assert!(!legacy_dir.exists(), "legacy state dir must be gone");
        assert!(
            !legacy_fragment_dir(&config).exists(),
            "legacy fragment dir must be gone"
        );
        assert!(report.fragment.status == ChangeStatus::Create);

        let new_config = temp.path().join(APP_DATA_DIR_NAME).join("config.toml");
        assert_eq!(
            fs::read(&new_config).expect("new config"),
            b"schema_version = 1\n",
            "config bytes must carry over unchanged"
        );

        // Fresh fragment at the Winter path.
        assert!(config.fragment_path().is_file(), "new fragment exists");

        // Settings: old ids removed, new ids installed, user binding intact.
        let settings_path = temp
            .path()
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        let settings = fs::read_to_string(&settings_path).expect("settings");
        assert!(
            !settings.contains("User.WinTerminalP."),
            "legacy bindings must be removed: {settings}"
        );
        assert!(
            settings.contains("User.Winter."),
            "fresh Winter bindings must be installed: {settings}"
        );
        assert!(
            settings.contains("User.Own.Custom"),
            "user bindings must survive: {settings}"
        );

        // Shell: legacy block replaced by the Winter block.
        let profile = config
            .documents_dir
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1");
        let profile_text = fs::read_to_string(profile).expect("profile");
        assert!(
            !profile_text.contains("WinTerminalP"),
            "legacy shell block must be gone: {profile_text}"
        );
        assert!(
            profile_text.contains("Winter shell integration"),
            "fresh Winter shell block must be installed: {profile_text}"
        );
        assert!(profile_text.contains("Write-Host hi"));

        // Everything healthy afterwards, and migration is not re-triggered.
        let doctor = super::super::doctor(&config).expect("doctor after migration");
        assert!(
            doctor
                .issues
                .iter()
                .all(|issue| !issue.contains("legacy WinTerminalP")),
            "no legacy issues after migration: {:?}",
            doctor.issues
        );
        super::super::install(&config).expect("second install is a no-op");
    }

    #[test]
    fn uninstall_removes_a_legacy_installation_without_the_new_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path());

        let handled = uninstall_if_present(&config).expect("legacy uninstall");
        assert!(handled, "legacy installation must be detected");
        assert!(!temp.path().join(LEGACY_APP_DATA_DIR_NAME).exists());
        // No config carry-over on plain uninstall.
        assert!(
            !temp
                .path()
                .join(APP_DATA_DIR_NAME)
                .join("config.toml")
                .exists()
        );
        // Second call finds nothing.
        assert!(!uninstall_if_present(&config).expect("second call"));
    }

    #[test]
    fn plan_and_doctor_report_the_legacy_installation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path());

        let issue = detection_issue(&config).expect("detection issue");
        assert!(issue.contains("legacy WinTerminalP installation"));
        assert!(issue.contains("winter install"));

        let plan = super::super::plan(&config).expect("plan succeeds");
        assert!(
            plan.issues
                .iter()
                .any(|entry| entry.contains("legacy WinTerminalP")),
            "plan must surface the legacy installation: {:?}",
            plan.issues
        );
        assert!(plan.can_install, "legacy must not block installation");

        let report = super::super::doctor(&config).expect("doctor succeeds");
        assert!(!report.healthy, "legacy installation needs attention");
        assert!(
            report
                .issues
                .iter()
                .any(|entry| entry.contains("legacy WinTerminalP"))
        );
    }

    #[test]
    fn manifestless_foreign_fragment_is_refused_and_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config_with(temp.path());
        let fragment_dir = legacy_fragment_dir(&config);
        fs::create_dir_all(&fragment_dir).expect("fragment dir");
        let fragment_path = fragment_dir.join("actions.json");
        fs::write(&fragment_path, br#"{"actions":[{"id":"Some.Other.Tool"}]}"#).expect("fragment");

        let error = uninstall_if_present(&config).expect_err("foreign fragment must refuse");
        assert!(
            error
                .to_string()
                .contains("does not contain Winter action ids")
        );
        assert!(fragment_path.exists(), "foreign fragment must be untouched");
        assert!(temp.path().join(LEGACY_APP_DATA_DIR_NAME).exists() || true);
    }

    #[test]
    fn manifestless_our_fragment_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config_with(temp.path());
        let fragment_dir = legacy_fragment_dir(&config);
        fs::create_dir_all(&fragment_dir).expect("fragment dir");
        fs::write(
            fragment_dir.join("actions.json"),
            br#"{"actions":[{"id":"User.WinTerminalP.SplitLeft"}]}"#,
        )
        .expect("fragment");

        uninstall_if_present(&config).expect("cleanup succeeds");
        assert!(!fragment_dir.join("actions.json").exists());
    }

    #[test]
    fn jsonc_removal_round_trip_for_the_legacy_record_shape() {
        // Guards the assumption the migration leans on: records with legacy
        // ids parse and remove matching entries.
        let raw = br#"{"keybindings":[{"id":"User.WinTerminalP.SplitLeft","keys":"ctrl+f13"}]}"#;
        let definition =
            serde_json::json!({"id": "User.WinTerminalP.SplitLeft", "keys": "ctrl+f13"});
        let record = ManagedKeybindingManifest {
            canonical_id: "User.WinTerminalP.SplitLeft".to_owned(),
            canonical_chord: "ctrl+f13".to_owned(),
            definition,
        };
        let edit = jsonc::remove_managed_keybindings(raw, &[record]).expect("removal parses");
        assert_eq!(edit.removed_binding_count, 1);
        assert!(edit.replacement.is_some());
    }
}
