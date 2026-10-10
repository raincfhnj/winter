//! One-shot cleanup of pre-Winter installations.
//!
//! Older releases branded their on-disk world `WinTerminalPP` (and briefly
//! `WinTerminalP`): a state directory under `%LOCALAPPDATA%`, a fragment
//! under `Fragments\<name>`, `User.WinTerminalPP.*` action ids in
//! `settings.json`, and `WinTerminalP` shell markers. Current code only knows
//! the Winter-branded identifiers, so [`detect`] finds every historical
//! directory and [`migrate`] removes the old world with two complementary
//! passes:
//!
//! 1. the legacy manifest's own records drive precise removal (hash-checked,
//!    user-modified entries preserved) — when a manifest exists;
//! 2. a prefix sweep strips `user.winterminalp*.` ids from every discovered
//!    settings file, covering channels the manifest never recorded.
//!
//! The config is carried over byte-for-byte, and the legacy state directory —
//! the only recovery record — is deleted only after every step succeeds.

use std::fs;
use std::path::PathBuf;

use super::fragment::{execute_uninstall_fragment, plan_uninstall_fragment};
use super::manifest::{IntegrationManifest, load_manifest};
use super::targets::{execute_uninstall_target, plan_uninstall_target};
use super::transaction::{
    atomic_replace, create_backup, read_optional_snapshot, remove_if_hash, sha256_hex,
};
use super::types::APP_DATA_DIR_NAME;
use super::{ChangeStatus, IntegrationConfig, discover_targets, jsonc, shell};
use crate::{AppError, AppResult};

/// Historical state/fragment directory names, newest first.
const LEGACY_DIR_NAMES: [&str; 2] = ["WinTerminalPP", "WinTerminalP"];

/// Lowercase id prefixes of every historical action namespace.
const LEGACY_ID_PREFIXES: [&str; 2] = ["user.winterminalpp.", "user.winterminalp."];

/// Snapshot of the historical installation, if any artifacts exist.
pub(super) struct LegacyInstallation {
    /// Existing `%LOCALAPPDATA%\<name>` state directories.
    dirs: Vec<PathBuf>,
    /// Existing `Fragments\<name>` directories.
    fragment_dirs: Vec<PathBuf>,
    /// First readable legacy manifest among the state directories.
    manifest: Option<super::manifest::LoadedManifest>,
}

fn state_dir(config: &IntegrationConfig, name: &str) -> PathBuf {
    config.local_app_data.join(name)
}

fn fragment_dir(config: &IntegrationConfig, name: &str) -> PathBuf {
    config
        .local_app_data
        .join("Microsoft")
        .join("Windows Terminal")
        .join("Fragments")
        .join(name)
}

/// Returns the legacy installation when any historical artifact exists.
///
/// A present-but-unreadable legacy manifest propagates as an error: the
/// migration must not guess without its ownership record.
pub(super) fn detect(config: &IntegrationConfig) -> AppResult<Option<LegacyInstallation>> {
    let mut dirs = Vec::new();
    let mut fragment_dirs = Vec::new();
    for name in LEGACY_DIR_NAMES {
        let dir = state_dir(config, name);
        if dir.exists() {
            dirs.push(dir);
        }
        let fragment = fragment_dir(config, name);
        if fragment.exists() {
            fragment_dirs.push(fragment);
        }
    }
    if dirs.is_empty() && fragment_dirs.is_empty() {
        return Ok(None);
    }
    let mut manifest = None;
    for dir in &dirs {
        let manifest_path = dir.join("integration").join("manifest.json");
        if manifest_path.is_file() {
            manifest = Some(load_manifest(&manifest_path)?);
            break;
        }
    }
    Ok(Some(LegacyInstallation {
        dirs,
        fragment_dirs,
        manifest,
    }))
}

/// Non-fatal detection for `plan`/`doctor`: an issue string when a legacy
/// installation (or an unreadable one) is present, `None` otherwise.
pub(super) fn detection_issue(config: &IntegrationConfig) -> Option<String> {
    match detect(config) {
        Ok(Some(legacy)) => Some(format!(
            "legacy installation detected ({}); run 'winter install' to migrate",
            legacy
                .dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Ok(None) => None,
        Err(error) => Some(format!(
            "legacy installation is present but unreadable: {error}; \
             inspect and delete {} manually",
            config.local_app_data.join(LEGACY_DIR_NAMES[0]).display()
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
        "legacy installation detected ({}); migrating to Winter",
        legacy
            .dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
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
    eprintln!("legacy installation detected; removing it");
    clean(config, &legacy, false)?;
    Ok(true)
}

fn clean(
    config: &IntegrationConfig,
    legacy: &LegacyInstallation,
    carry_config: bool,
) -> AppResult<()> {
    // 1. Precise removal through the legacy manifest's own records: ids are
    //    data, so the current code never has to spell the old tokens.
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
    }

    // 2. Manifest-less fragment removal: only when unmistakably ours.
    for fragment_dir in &legacy.fragment_dirs {
        let fragment_path = fragment_dir.join("actions.json");
        let Ok(bytes) = fs::read(&fragment_path) else {
            continue;
        };
        if String::from_utf8_lossy(&bytes)
            .to_ascii_lowercase()
            .contains("user.winterminalp")
        {
            let expected = sha256_hex(&bytes);
            if !remove_if_hash(&fragment_path, &expected)? {
                return Err(AppError::SettingsConflict(format!(
                    "{} changed while migrating; rerun 'winter install'",
                    fragment_path.display()
                )));
            }
        } else if legacy.manifest.is_none() {
            return Err(AppError::SettingsConflict(format!(
                "{} exists without a legacy manifest and does not contain Winter action \
                 ids; inspect and remove it manually",
                fragment_path.display()
            )));
        }
    }

    // 3. Prefix sweep over every discovered settings file: catches channels
    //    the manifest never recorded (Preview/Canary installs) and ids the
    //    record pass preserved as user-modified.
    for target in discover_targets(config) {
        let Some(snapshot) = read_optional_snapshot(&target.settings_path)? else {
            continue;
        };
        let edit = jsonc::remove_legacy_keybindings(&snapshot.bytes, &LEGACY_ID_PREFIXES)?;
        let Some(replacement) = edit.replacement else {
            continue;
        };
        create_backup(
            &config.state_dir,
            "legacy-settings",
            &target.settings_path,
            &snapshot,
        )?;
        atomic_replace(&target.settings_path, Some(&snapshot.sha256), &replacement)?;
        eprintln!(
            "  removed {} legacy keybinding(s) from {}",
            edit.removed_binding_count,
            target.settings_path.display()
        );
    }

    // 4. Legacy shell blocks (force removal of namespaced markers).
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

    // 5. Carry the user's config into the new location before the old tree
    //    disappears (copy, not move — a later failure keeps the original).
    if carry_config {
        let new_config = new_config_path(config);
        if !new_config.exists()
            && let Some(legacy_config) = legacy
                .dirs
                .iter()
                .map(|dir| dir.join("config.toml"))
                .find(|path| path.is_file())
        {
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

    // 6. Only now are the legacy trees disposable.
    for dir in &legacy.dirs {
        if dir.exists() {
            fs::remove_dir_all(dir)
                .map_err(|error| AppError::io("remove legacy state directory", dir, error))?;
        }
    }
    for dir in &legacy.fragment_dirs {
        // Individual files were removed above; the directory is ours by
        // namespace, so drop it when empty and leave anything else alone.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

fn new_config_path(config: &IntegrationConfig) -> PathBuf {
    config
        .local_app_data
        .join(APP_DATA_DIR_NAME)
        .join("config.toml")
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

    /// Builds the full legacy world for the given historical directory name:
    /// state dir + manifest, discoverable settings with old ids, fragment,
    /// legacy shell block, and a legacy config file. An extra `extra_settings`
    /// path seeds a channel the manifest does NOT record.
    fn seed_legacy(
        root: &Path,
        legacy_name: &str,
        extra_settings: Option<&Path>,
    ) -> IntegrationConfig {
        seed_legacy_with_markers(
            root,
            legacy_name,
            extra_settings,
            ("WinTerminalP", "WinTerminalP"),
        )
    }

    /// [`seed_legacy`] with an explicit shell-marker branding.
    ///
    /// The abandoned project was branded `WinTerminalPP` and its shell markup
    /// said so; only the state directory and fragment were ever renamed to the
    /// single-`P` spelling. Both spellings exist in the wild, so the fixture
    /// must be able to seed either one.
    fn seed_legacy_with_markers(
        root: &Path,
        legacy_name: &str,
        extra_settings: Option<&Path>,
        (marker_begin, marker_end): (&str, &str),
    ) -> IntegrationConfig {
        let config = config_with(root);
        let legacy_dir = root.join(legacy_name);

        fs::create_dir_all(&legacy_dir).expect("legacy dir");
        fs::write(legacy_dir.join("config.toml"), b"schema_version = 1\n").expect("legacy config");

        let settings_path = root
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).expect("settings dir");
        let settings_bytes = br#"{"keybindings":[
            {"id":"User.WinTerminalPP.SplitLeft","keys":"ctrl+alt+shift+f13"},
            {"id":"User.Own.Custom","keys":"ctrl+f9"}
        ]}"#;
        fs::write(&settings_path, settings_bytes).expect("settings");
        let settings_snapshot = read_snapshot(&settings_path).expect("settings snapshot");

        if let Some(extra) = extra_settings {
            fs::create_dir_all(extra.parent().unwrap()).expect("extra settings dir");
            fs::write(
                extra,
                br#"{"keybindings":[
                {"id":"User.WinTerminalPP.NewTab","keys":"ctrl+alt+shift+f18"}
            ]}"#,
            )
            .expect("extra settings");
        }

        let fragment_dir = fragment_dir(&config, legacy_name);
        fs::create_dir_all(&fragment_dir).expect("fragment dir");
        let fragment_path = fragment_dir.join("actions.json");
        let fragment_bytes =
            br#"{"actions":[{"id":"User.WinTerminalPP.SplitLeft","keys":"ctrl+alt+shift+f13"}]}"#;
        fs::write(&fragment_path, fragment_bytes).expect("fragment");
        let fragment_sha = sha256_hex(fragment_bytes);

        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile dir");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let legacy_block = format!(
            "# >>> {marker_begin} shell integration >>>\n\
             $Global:__WinTerminalPP_PromptWrapped = $true\n\
             # <<< {marker_end} shell integration <<<\n"
        );
        fs::write(&profile, format!("Write-Host hi\n{legacy_block}")).expect("profile");

        let definition = serde_json::json!({
            "id": "User.WinTerminalPP.SplitLeft",
            "keys": "ctrl+alt+shift+f13",
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
                    canonical_id: "user.winterminalpp.splitleft".to_owned(),
                    canonical_chord: "ctrl+shift+alt+f13".to_owned(),
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

    fn wt_settings(root: &Path, channel: &str) -> PathBuf {
        root.join("Packages")
            .join(channel)
            .join("LocalState")
            .join("settings.json")
    }

    #[test]
    fn install_migrates_win_terminal_pp_world_and_installs_winter_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path(), "WinTerminalPP", None);

        let report = super::super::install(&config).expect("install migrates then installs");

        assert!(!temp.path().join("WinTerminalPP").exists());
        assert!(!temp.path().join("WinTerminalP").exists());
        assert!(!fragment_dir(&config, "WinTerminalPP").exists());
        assert!(report.fragment.status == ChangeStatus::Create);

        assert_eq!(
            fs::read(new_config_path(&config)).expect("new config"),
            b"schema_version = 1\n",
            "config bytes must carry over unchanged"
        );
        assert!(config.fragment_path().is_file(), "new fragment exists");

        let settings = fs::read_to_string(wt_settings(
            temp.path(),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
        ))
        .expect("settings");
        assert!(
            !settings.to_ascii_lowercase().contains("user.winterminalp"),
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

        let profile = config
            .documents_dir
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1");
        let profile_text = fs::read_to_string(profile).expect("profile");
        assert!(
            !profile_text.contains("WinTerminalPP"),
            "no legacy marker spelling may survive: {profile_text}"
        );
        assert!(profile_text.contains("Winter shell integration"));
        assert!(profile_text.contains("Write-Host hi"));

        let doctor = super::super::doctor(&config).expect("doctor after migration");
        assert!(
            doctor
                .issues
                .iter()
                .all(|issue| !issue.contains("legacy installation")),
            "no legacy issues after migration: {:?}",
            doctor.issues
        );
        super::super::install(&config).expect("second install is a no-op");
    }

    /// The abandoned project was branded `WinTerminalPP`, and the shell markup
    /// it wrote carried that spelling. Sweeping only the single-`P` markers left
    /// a live prompt wrapper behind next to the current Winter block, while
    /// `doctor` reported a clean migration.
    #[test]
    fn double_p_shell_markers_are_swept_too() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy_with_markers(
            temp.path(),
            "WinTerminalPP",
            None,
            ("WinTerminalPP", "WinTerminalPP"),
        );

        super::super::install(&config).expect("install migrates then installs");

        let profile = config
            .documents_dir
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1");
        let profile_text = fs::read_to_string(&profile).expect("profile");
        assert!(
            !profile_text.contains("WinTerminalPP") && !profile_text.contains("WinTerminalP"),
            "both historical marker spellings must be swept: {profile_text}"
        );
        assert!(
            profile_text.contains("Write-Host hi"),
            "unmanaged profile content must survive: {profile_text}"
        );
        assert_eq!(
            profile_text.matches("Winter shell integration").count(),
            2,
            "exactly one current block must remain: {profile_text}"
        );

        // `install` adds the current Winter block afterwards, so the final
        // report entry for this profile is an `Update`. The sweep leaves its
        // own evidence: a `legacy-shell` backup of the bytes it removed.
        let legacy_backups = fs::read_dir(config.state_dir.join("backups"))
            .expect("backups directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("legacy-shell.")
            })
            .count();
        assert_eq!(
            legacy_backups, 1,
            "the double-P block must have been swept through a legacy-shell backup"
        );
    }

    #[test]
    fn stray_ids_in_unrecorded_channels_are_swept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let preview = wt_settings(
            temp.path(),
            "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
        );
        let config = seed_legacy(temp.path(), "WinTerminalPP", Some(&preview));

        migrate_if_present(&config).expect("migration succeeds");

        let preview_text = fs::read_to_string(&preview).expect("preview settings");
        assert!(
            !preview_text
                .to_ascii_lowercase()
                .contains("user.winterminalp"),
            "unrecorded channel must be swept: {preview_text}"
        );
        assert!(!temp.path().join("WinTerminalPP").exists());
    }

    #[test]
    fn uninstall_removes_a_legacy_installation_without_the_new_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path(), "WinTerminalP", None);

        let handled = uninstall_if_present(&config).expect("legacy uninstall");
        assert!(handled, "legacy installation must be detected");
        assert!(!temp.path().join("WinTerminalP").exists());
        assert!(
            !new_config_path(&config).exists(),
            "no config carry-over on plain uninstall"
        );
        assert!(!uninstall_if_present(&config).expect("second call"));
    }

    #[test]
    fn plan_and_doctor_report_the_legacy_installation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = seed_legacy(temp.path(), "WinTerminalPP", None);

        let issue = detection_issue(&config).expect("detection issue");
        assert!(issue.contains("legacy installation detected"));
        assert!(issue.contains("winter install"));

        let plan = super::super::plan(&config).expect("plan succeeds");
        assert!(
            plan.issues
                .iter()
                .any(|entry| entry.contains("legacy installation")),
            "plan must surface the legacy installation: {:?}",
            plan.issues
        );
        // Plan reflects the current state: legacy ids still occupy the bridge
        // chords, so it reports conflicts until `winter install` migrates
        // (install runs the migration before its own conflict analysis).
        assert!(
            !plan.can_install,
            "pre-migration plan shows the occupied chords as conflicts"
        );

        let report = super::super::doctor(&config).expect("doctor succeeds");
        assert!(!report.healthy, "legacy installation needs attention");
        assert!(
            report
                .issues
                .iter()
                .any(|entry| entry.contains("legacy installation"))
        );
    }

    #[test]
    fn manifestless_foreign_fragment_is_refused_and_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config_with(temp.path());
        let dir = fragment_dir(&config, "WinTerminalPP");
        fs::create_dir_all(&dir).expect("fragment dir");
        let fragment_path = dir.join("actions.json");
        fs::write(&fragment_path, br#"{"actions":[{"id":"Some.Other.Tool"}]}"#).expect("fragment");

        let error = uninstall_if_present(&config).expect_err("foreign fragment must refuse");
        assert!(
            error
                .to_string()
                .contains("does not contain Winter action ids")
        );
        assert!(fragment_path.exists(), "foreign fragment must be untouched");
    }

    #[test]
    fn manifestless_our_fragment_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config_with(temp.path());
        let dir = fragment_dir(&config, "WinTerminalPP");
        fs::create_dir_all(&dir).expect("fragment dir");
        fs::write(
            dir.join("actions.json"),
            br#"{"actions":[{"id":"User.WinTerminalPP.SplitLeft"}]}"#,
        )
        .expect("fragment");

        uninstall_if_present(&config).expect("cleanup succeeds");
        assert!(!dir.join("actions.json").exists());
    }

    #[test]
    fn remove_legacy_keybindings_targets_only_historical_namespaces() {
        let raw = br#"{"keybindings":[
            {"id":"User.WinTerminalPP.SplitLeft","keys":"ctrl+alt+shift+f13"},
            {"id":"User.WinTerminalP.Old","keys":"ctrl+f13"},
            {"id":"User.Winter.SplitLeft","keys":"ctrl+f13"},
            {"id":"User.Own.Custom","keys":"ctrl+f9"}
        ]}"#;
        let edit =
            jsonc::remove_legacy_keybindings(raw, &LEGACY_ID_PREFIXES).expect("sweep parses");
        assert_eq!(edit.removed_binding_count, 2);
        let text = String::from_utf8(edit.replacement.expect("replacement")).expect("utf8");
        assert!(!text.to_ascii_lowercase().contains("user.winterminalp"));
        assert!(text.contains("User.Winter.SplitLeft"));
        assert!(text.contains("User.Own.Custom"));

        let clean = jsonc::remove_legacy_keybindings(
            br#"{"keybindings":[{"id":"User.Winter.X","keys":"ctrl+f13"}]}"#,
            &LEGACY_ID_PREFIXES,
        )
        .expect("clean parse");
        assert!(clean.replacement.is_none(), "nothing legacy to remove");
    }
}
