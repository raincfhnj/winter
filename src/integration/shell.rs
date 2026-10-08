//! Managed PowerShell shell integration.
//!
//! Windows Terminal only inherits the working directory of a duplicated pane
//! when the shell reports it through the `OSC 9;9` sequence. Winter
//! therefore appends a small, reversible prompt wrapper to the PowerShell
//! profiles so that `splitPane` with `splitMode: duplicate` starts in the same
//! directory as the focused pane.

use std::path::{Path, PathBuf};

use crate::{AppError, AppResult};

use super::encoding::{UTF16LE_BOM, Utf16LeError, decode_utf16_le, encode_utf16_le_with_bom};
use super::transaction::{atomic_replace, create_backup, read_optional_snapshot};
use super::types::{ChangeStatus, IntegrationConfig, ShellIntegrationReport, ShellKind};

const BEGIN_MARKER: &str = "# >>> Winter shell integration >>>";
const END_MARKER: &str = "# <<< Winter shell integration <<<";

/// Historical marker pair written by pre-Winter releases; referenced only by
/// the one-shot migration in [`super::legacy`].
pub(super) const LEGACY_BEGIN_MARKER: &str = "# >>> WinTerminalP shell integration >>>";
pub(super) const LEGACY_END_MARKER: &str = "# <<< WinTerminalP shell integration <<<";

const SNIPPET_BODY: &str = r#"# Managed by Winter. Run `winter uninstall` to remove this block.
if (-not $Global:__Winter_PromptWrapped) {
    $Global:__Winter_PromptWrapped = $true
    $Global:__Winter_OriginalPrompt = $function:prompt
    function global:prompt {
        $__winterLocation = $ExecutionContext.SessionState.Path.CurrentLocation
        $__winterOsc = "$([char]27)]9;9;`"$__winterLocation`"$([char]7)"
        $__winterBase = if ($Global:__Winter_OriginalPrompt) {
            & $Global:__Winter_OriginalPrompt
        }
        else {
            "PS $__winterLocation> "
        }
        if ($__winterBase -is [System.Array]) {
            return @($__winterOsc) + @($__winterBase)
        }
        return $__winterOsc + [string]$__winterBase
    }
}"#;

fn managed_block(newline: &str) -> String {
    let body = SNIPPET_BODY.replace("\r\n", "\n").replace('\n', newline);
    format!("{BEGIN_MARKER}{newline}{body}{newline}{END_MARKER}")
}

fn profile_targets(config: &IntegrationConfig) -> [(ShellKind, PathBuf); 2] {
    [
        (
            ShellKind::WindowsPowerShell,
            config
                .documents_dir
                .join("WindowsPowerShell")
                .join("Microsoft.PowerShell_profile.ps1"),
        ),
        (
            ShellKind::PowerShell,
            config
                .documents_dir
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1"),
        ),
    ]
}

/// Plans both profiles. Per-profile failures are reported on that profile's
/// entry — content conflicts as `Conflict`, environmental I/O failures as
/// `Skipped` — instead of aborting the whole command, so a broken profile
/// never blocks the Terminal action bridge.
pub(crate) fn plan(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| plan_profile(shell, &path))
        .collect()
}

pub(crate) fn install(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| install_profile(config, shell, &path))
        .collect()
}

pub(crate) fn uninstall(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| uninstall_profile(config, shell, &path))
        .collect()
}

fn plan_profile(shell: ShellKind, path: &Path) -> ShellIntegrationReport {
    plan_profile_inner(shell, path).unwrap_or_else(|error| shell_error(shell, path, error))
}

fn plan_profile_inner(shell: ShellKind, path: &Path) -> AppResult<ShellIntegrationReport> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(report(
            shell,
            path,
            skipped_or(ChangeStatus::Create, path),
            None,
            None,
        ));
    };
    let (_, text) = decode_profile(path, &snapshot.bytes)?;
    Ok(match block_state(&text, BEGIN_MARKER, END_MARKER) {
        BlockState::Absent => report(shell, path, ChangeStatus::Update, None, None),
        BlockState::Present { start, end } => {
            let desired = managed_block(detect_newline(&text));
            if normalized_eq(&text[start..end], &desired) {
                report(shell, path, ChangeStatus::Unchanged, None, None)
            } else {
                report(shell, path, ChangeStatus::Update, None, None)
            }
        }
        BlockState::Malformed(message) => {
            report(shell, path, ChangeStatus::Conflict, None, Some(message))
        }
    })
}

fn install_profile(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
) -> ShellIntegrationReport {
    install_profile_inner(config, shell, path)
        .unwrap_or_else(|error| shell_error(shell, path, error))
}

fn install_profile_inner(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
) -> AppResult<ShellIntegrationReport> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        if !path.parent().is_some_and(Path::is_dir) {
            return Ok(report(
                shell,
                path,
                ChangeStatus::Skipped,
                None,
                Some("profile folder is not present".to_owned()),
            ));
        }
        let block = managed_block("\r\n");
        let mut bytes = block.into_bytes();
        bytes.push(b'\r');
        bytes.push(b'\n');
        atomic_replace(path, None, &bytes)?;
        return Ok(report(shell, path, ChangeStatus::Create, None, None));
    };

    let (encoding, text) = decode_profile(path, &snapshot.bytes)?;
    let newline = detect_newline(&text);
    match block_state(&text, BEGIN_MARKER, END_MARKER) {
        BlockState::Malformed(message) => Ok(report(
            shell,
            path,
            ChangeStatus::Conflict,
            None,
            Some(message),
        )),
        BlockState::Present { start, end } => {
            let desired = managed_block(newline);
            if normalized_eq(&text[start..end], &desired) {
                return Ok(report(shell, path, ChangeStatus::Unchanged, None, None));
            }
            let backup = create_backup(&config.state_dir, "shell", path, &snapshot)?;
            let mut updated = String::with_capacity(text.len());
            updated.push_str(&text[..start]);
            updated.push_str(&desired);
            updated.push_str(&text[end..]);
            write_profile(path, encoding, &updated, Some(&snapshot.sha256))?;
            Ok(report(
                shell,
                path,
                ChangeStatus::Update,
                Some(backup.backup_path),
                None,
            ))
        }
        BlockState::Absent => {
            let backup = create_backup(&config.state_dir, "shell", path, &snapshot)?;
            let mut updated = text.clone();
            if !updated.is_empty() {
                updated.push_str(newline);
            }
            updated.push_str(&managed_block(newline));
            updated.push_str(newline);
            write_profile(path, encoding, &updated, Some(&snapshot.sha256))?;
            Ok(report(
                shell,
                path,
                ChangeStatus::Update,
                Some(backup.backup_path),
                None,
            ))
        }
    }
}

fn uninstall_profile(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
) -> ShellIntegrationReport {
    uninstall_profile_inner(config, shell, path)
        .unwrap_or_else(|error| shell_error(shell, path, error))
}

fn uninstall_profile_inner(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
) -> AppResult<ShellIntegrationReport> {
    uninstall_profile_with(
        config,
        shell,
        path,
        BEGIN_MARKER,
        END_MARKER,
        BlockPolicy::PreserveEdited,
    )
}

/// Removes the historical block written by pre-Winter releases.
///
/// The legacy markers are namespaced to this project, so a well-formed block
/// is removed even when its content was edited; the current-marker path keeps
/// edited blocks (see [`uninstall_profile_inner`]).
pub(super) fn uninstall_legacy(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| {
            uninstall_profile_with(
                config,
                shell,
                &path,
                LEGACY_BEGIN_MARKER,
                LEGACY_END_MARKER,
                BlockPolicy::RemoveEvenIfEdited,
            )
            .unwrap_or_else(|error| shell_error(shell, &path, error))
        })
        .collect()
}

/// Whether an edited managed block may be removed (`RemoveEvenIfEdited`, used
/// for namespaced legacy markers) or must be preserved (`PreserveEdited`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockPolicy {
    PreserveEdited,
    RemoveEvenIfEdited,
}

fn uninstall_profile_with(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
    begin_marker: &str,
    end_marker: &str,
    policy: BlockPolicy,
) -> AppResult<ShellIntegrationReport> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(report(
            shell,
            path,
            ChangeStatus::Missing,
            None,
            Some("profile is not present".to_owned()),
        ));
    };
    let (encoding, text) = decode_profile(path, &snapshot.bytes)?;
    match block_state(&text, begin_marker, end_marker) {
        BlockState::Absent => Ok(report(
            shell,
            path,
            ChangeStatus::Missing,
            None,
            Some("managed block is not present".to_owned()),
        )),
        BlockState::Malformed(message) => Ok(report(
            shell,
            path,
            ChangeStatus::Conflict,
            None,
            Some(message),
        )),
        BlockState::Present { start, end } => {
            let newline = detect_newline(&text);
            if policy == BlockPolicy::PreserveEdited
                && !normalized_eq(&text[start..end], &managed_block(newline))
            {
                return Ok(report(
                    shell,
                    path,
                    ChangeStatus::Preserved,
                    None,
                    Some("managed block was edited and was left in place".to_owned()),
                ));
            }
            let backup_label = if policy == BlockPolicy::RemoveEvenIfEdited {
                "legacy-shell"
            } else {
                "shell"
            };
            let backup = create_backup(&config.state_dir, backup_label, path, &snapshot)?;
            let mut block_start = start;
            if block_start >= newline.len() && text[..block_start].ends_with(newline) {
                block_start -= newline.len();
            }
            let mut block_end = end;
            if &text[block_end..] == newline {
                block_end += newline.len();
            }
            let mut updated = String::with_capacity(text.len());
            updated.push_str(&text[..block_start]);
            updated.push_str(&text[block_end..]);
            write_profile(path, encoding, &updated, Some(&snapshot.sha256))?;
            Ok(report(
                shell,
                path,
                ChangeStatus::Removed,
                Some(backup.backup_path),
                None,
            ))
        }
    }
}

fn report(
    shell: ShellKind,
    path: &Path,
    status: ChangeStatus,
    backup_path: Option<PathBuf>,
    message: Option<String>,
) -> ShellIntegrationReport {
    ShellIntegrationReport {
        shell,
        path: path.to_path_buf(),
        status,
        backup_path,
        message,
    }
}

/// Builds the per-profile failure report, never returning `Err` to callers.
///
/// Classification: environmental failures — [`AppError::Io`] (locked file,
/// permission denied, disk full), [`AppError::Platform`], and
/// [`AppError::Native`] (the `MoveFileExW` replace step) — map to
/// [`ChangeStatus::Skipped`], because nothing in the profile content
/// conflicts; the operation simply could not run. Everything else
/// ([`AppError::SettingsConflict`] compare-and-swap races, [`AppError::Settings`]
/// encoding/symlink refusals) keeps the historical `Conflict` so the user
/// inspects and resolves the content itself.
///
/// `error.to_string()` is the `AppError` `Display`, whose `{source}` rendering
/// already carries the top-level source-chain message for `Io`/`Platform`, so
/// the environmental diagnosis is preserved verbatim.
fn shell_error(shell: ShellKind, path: &Path, error: AppError) -> ShellIntegrationReport {
    let status = match &error {
        AppError::Io { .. } | AppError::Platform { .. } | AppError::Native(_) => {
            ChangeStatus::Skipped
        }
        _ => ChangeStatus::Conflict,
    };
    report(shell, path, status, None, Some(error.to_string()))
}

fn skipped_or(status: ChangeStatus, path: &Path) -> ChangeStatus {
    if path.parent().is_some_and(Path::is_dir) {
        status
    } else {
        ChangeStatus::Skipped
    }
}

/// Encoding of an existing profile. The managed block is written back in the
/// same encoding so non-ASCII profile content is never corrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileEncoding {
    Utf8 { bom: bool },
    Utf16Le,
}

fn decode_profile(path: &Path, bytes: &[u8]) -> AppResult<(ProfileEncoding, String)> {
    if let Some(content) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return Ok((
            ProfileEncoding::Utf8 { bom: true },
            decode_utf8(path, content)?,
        ));
    }
    if let Some(content) = bytes.strip_prefix(UTF16LE_BOM) {
        let text = decode_utf16_le(content).map_err(|error| AppError::Settings {
            path: path.to_path_buf(),
            message: match error {
                Utf16LeError::Truncated => {
                    "PowerShell profile has a truncated UTF-16LE byte sequence".to_owned()
                }
                Utf16LeError::Invalid => "PowerShell profile is not valid UTF-16LE".to_owned(),
            },
        })?;
        return Ok((ProfileEncoding::Utf16Le, text));
    }
    Ok((
        ProfileEncoding::Utf8 { bom: false },
        decode_utf8(path, bytes)?,
    ))
}

fn decode_utf8(path: &Path, content: &[u8]) -> AppResult<String> {
    std::str::from_utf8(content)
        .map(str::to_owned)
        .map_err(|_| AppError::Settings {
            path: path.to_path_buf(),
            message: "PowerShell profile is neither UTF-8 nor UTF-16LE".to_owned(),
        })
}

fn write_profile(
    path: &Path,
    encoding: ProfileEncoding,
    text: &str,
    expected_sha256: Option<&str>,
) -> AppResult<()> {
    let bytes = match encoding {
        ProfileEncoding::Utf8 { bom } => {
            let mut bytes = Vec::with_capacity(text.len() + usize::from(bom) * 3);
            if bom {
                bytes.extend_from_slice(b"\xef\xbb\xbf");
            }
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
        ProfileEncoding::Utf16Le => encode_utf16_le_with_bom(text),
    };
    atomic_replace(path, expected_sha256, &bytes)?;
    Ok(())
}

fn detect_newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Equality check that treats `\r\n` and `\n` as equivalent, with a
/// zero-allocation fast path when both sides are byte-identical.
fn normalized_eq(left: &str, right: &str) -> bool {
    left == right || left.replace("\r\n", "\n") == right.replace("\r\n", "\n")
}

enum BlockState {
    Absent,
    Present { start: usize, end: usize },
    Malformed(String),
}

/// Classifies every marker occurrence instead of only the first pair, so a
/// stray, reversed, or duplicated marker cannot slip past install/uninstall and
/// leave a silently surviving second block behind.
fn block_state(text: &str, begin_marker: &str, end_marker: &str) -> BlockState {
    let mut begins = text.match_indices(begin_marker).map(|(index, _)| index);
    let mut ends = text.match_indices(end_marker).map(|(index, _)| index);
    let begin = begins.next();
    let end = ends.next();
    let begin_count = usize::from(begin.is_some()) + begins.count();
    let end_count = usize::from(end.is_some()) + ends.count();
    match (begin, end) {
        (None, None) => BlockState::Absent,
        (Some(begin), Some(end)) if begin_count == 1 && end_count == 1 && end >= begin => {
            BlockState::Present {
                start: begin,
                end: end + end_marker.len(),
            }
        }
        _ => {
            let mut message = format!(
                "profile contains {begin_count} BEGIN marker(s) and {end_count} END marker(s); \
                 exactly one managed marker block pair is required"
            );
            if begin_count == 1 && end_count == 1 {
                message.push_str("; the END marker appears before the BEGIN marker");
            }
            BlockState::Malformed(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn config_with(root: &Path) -> IntegrationConfig {
        IntegrationConfig::new(root, root.join("state"), root.join("Documents"))
    }

    #[test]
    fn install_appends_a_single_managed_block_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, "Set-Alias ll Get-ChildItem\n")
            .expect("user profile should be written");

        let first = install(&config);
        let windows = first
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Update);

        let text = fs::read_to_string(&profile).expect("profile should be readable");
        assert_eq!(text.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(text.matches(END_MARKER).count(), 1);
        assert!(text.contains("]9;9;"));
        assert!(text.contains("Set-Alias ll Get-ChildItem"));

        let second = install(&config);
        let windows = second
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Unchanged);
    }

    #[test]
    fn uninstall_removes_only_the_managed_block() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, "Set-Alias ll Get-ChildItem\n")
            .expect("user profile should be written");
        install(&config);

        let report = uninstall(&config);
        let windows = report
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Removed);

        let text = fs::read_to_string(&profile).expect("profile should be readable");
        assert!(!text.contains(BEGIN_MARKER));
        assert!(text.contains("Set-Alias ll Get-ChildItem"));
    }

    #[test]
    fn uninstall_preserves_a_user_edited_block() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, "").expect("empty profile should be written");
        install(&config);
        let edited = fs::read_to_string(&profile)
            .expect("profile should be readable")
            .replace("PS $__winterLocation> ", "PS> ");
        fs::write(&profile, edited).expect("edited profile should be written");

        let report = uninstall(&config);
        let windows = report
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Preserved);
        assert!(
            fs::read_to_string(&profile)
                .expect("profile should remain readable")
                .contains(BEGIN_MARKER)
        );
    }

    #[test]
    fn install_skips_a_shell_without_a_profile_folder() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        fs::create_dir_all(config.documents_dir.join("WindowsPowerShell"))
            .expect("profile directory should be created");

        let report = install(&config);
        let powershell = report
            .iter()
            .find(|entry| entry.shell == ShellKind::PowerShell)
            .expect("powershell target");
        assert_eq!(powershell.status, ChangeStatus::Skipped);
    }

    #[test]
    fn utf16le_profile_is_preserved_and_written_back_as_utf16le() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let original = "Set-Alias é Get-ChildItem\r\n";
        let mut bytes = vec![0xff, 0xfe];
        for unit in original.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&profile, &bytes).expect("utf16 profile should be written");

        let report = install(&config);
        let windows = report
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Update);

        let written = fs::read(&profile).expect("profile should be readable");
        assert_eq!(&written[..2], &[0xff, 0xfe]);
        let text = String::from_utf16(
            &written[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>(),
        )
        .expect("utf16 profile should decode");
        assert!(text.contains("Set-Alias é Get-ChildItem"));
        assert!(text.contains("]9;9;"));
    }

    #[test]
    fn unsupported_profile_encoding_is_reported_without_blocking() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, b"\x81\x8d invalid windows-1252").expect("profile should be written");

        let report = install(&config);
        let windows = report
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target");
        assert_eq!(windows.status, ChangeStatus::Conflict);
        assert_eq!(
            fs::read(&profile).expect("profile should remain readable"),
            b"\x81\x8d invalid windows-1252"
        );
    }

    fn windows_report(reports: &[ShellIntegrationReport]) -> &ShellIntegrationReport {
        reports
            .iter()
            .find(|entry| entry.shell == ShellKind::WindowsPowerShell)
            .expect("windows powershell target")
    }

    #[test]
    fn install_then_uninstall_restores_the_original_bytes() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");

        let originals = [
            "Set-Alias ll Get-ChildItem\n",
            "Set-Alias ll Get-ChildItem",
            "Set-Alias ll Get-ChildItem\r\n",
            "Set-Alias a Get-ChildItem\r\nSet-Alias b Get-ChildItem",
            "",
        ];
        for original in originals {
            fs::write(&profile, original).expect("profile should be written");
            let before = fs::read(&profile).expect("profile should be readable");
            for cycle in 0..2 {
                let installed = install(&config);
                assert_eq!(
                    windows_report(&installed).status,
                    ChangeStatus::Update,
                    "install should update {original:?} on cycle {cycle}"
                );
                let removed = uninstall(&config);
                assert_eq!(
                    windows_report(&removed).status,
                    ChangeStatus::Removed,
                    "uninstall should remove {original:?} on cycle {cycle}"
                );
                assert_eq!(
                    fs::read(&profile).expect("profile should be readable"),
                    before,
                    "cycle {cycle} for {original:?} must restore the original bytes"
                );
            }
        }
    }

    #[test]
    fn crlf_profile_gets_a_fully_crlf_managed_block() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, "Set-Alias ll Get-ChildItem\r\n").expect("profile should be written");

        install(&config);

        let text = fs::read_to_string(&profile).expect("profile should be readable");
        assert_eq!(
            text.matches('\n').count(),
            text.matches("\r\n").count(),
            "every LF in a CRLF profile must be part of a CRLF sequence"
        );
        assert!(
            !text.contains("\r\r"),
            "the injected block must not double carriage returns"
        );
        assert!(text.contains(&managed_block("\r\n")));
    }

    #[test]
    fn environmental_read_failure_is_reported_as_skipped_not_conflict() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        // A directory at the profile path makes every read fail with a
        // permission-denied I/O error, which is environmental, not a conflict.
        fs::create_dir(&profile).expect("profile path should become a directory");

        for (label, reports) in [
            ("plan", plan(&config)),
            ("install", install(&config)),
            ("uninstall", uninstall(&config)),
        ] {
            let windows = windows_report(&reports);
            assert_eq!(
                windows.status,
                ChangeStatus::Skipped,
                "{label} must classify an I/O failure as skipped, not conflict"
            );
            let message = windows
                .message
                .as_deref()
                .expect("environmental failure should be explained");
            assert!(
                message.contains("I/O operation")
                    && message.contains("read integration file")
                    && message.contains(&profile.display().to_string()),
                "{label} should carry the AppError::Io display verbatim, got: {message}"
            );
            assert!(
                windows.backup_path.is_none(),
                "{label} must not report a backup for a failed read"
            );
        }
        assert!(
            profile.is_dir(),
            "an unreadable profile must survive untouched"
        );
    }

    #[test]
    fn environmental_backup_failure_during_install_is_reported_as_skipped() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        fs::write(&profile, "Set-Alias ll Get-ChildItem\n").expect("profile should be written");
        let before = fs::read(&profile).expect("profile should be readable");
        // A regular file where the state directory belongs makes backup
        // creation fail with an I/O error before the profile is rewritten.
        fs::write(&config.state_dir, "not a directory").expect("state file should be written");

        let report = install(&config);
        let windows = windows_report(&report);
        assert_eq!(
            windows.status,
            ChangeStatus::Skipped,
            "a backup I/O failure is environmental, not a content conflict"
        );
        let message = windows
            .message
            .as_deref()
            .expect("environmental failure should be explained");
        assert!(
            message.contains("I/O operation")
                && message.contains("create integration backup directory"),
            "message should carry the AppError::Io display verbatim, got: {message}"
        );
        assert_eq!(
            fs::read(&profile).expect("profile should remain readable"),
            before,
            "a profile must not be rewritten when its backup cannot be created"
        );
    }

    #[test]
    fn stray_end_marker_refuses_plan_install_and_uninstall_with_counts() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let original = format!(
            "Set-Alias ll Get-ChildItem\n{END_MARKER}\n{}\n",
            managed_block("\n")
        );
        fs::write(&profile, &original).expect("profile should be written");
        let before = fs::read(&profile).expect("profile should be readable");

        for (label, reports) in [
            ("plan", plan(&config)),
            ("install", install(&config)),
            ("uninstall", uninstall(&config)),
        ] {
            let windows = windows_report(&reports);
            assert_eq!(
                windows.status,
                ChangeStatus::Conflict,
                "{label} should refuse a stray END marker"
            );
            let message = windows
                .message
                .as_deref()
                .expect("conflict should explain the marker counts");
            assert!(
                message.contains("1 BEGIN marker(s) and 2 END marker(s)"),
                "{label} should count every marker, got: {message}"
            );
        }
        assert_eq!(
            fs::read(&profile).expect("profile should remain readable"),
            before,
            "a refused profile must not be rewritten"
        );
    }

    #[test]
    fn duplicate_managed_blocks_are_refused_with_counts() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let profile_dir = config.documents_dir.join("WindowsPowerShell");
        fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let block = managed_block("\n");
        let original = format!("Set-Alias ll Get-ChildItem\n{block}\n{block}\n");
        fs::write(&profile, &original).expect("profile should be written");
        let before = fs::read(&profile).expect("profile should be readable");

        for (label, reports) in [
            ("plan", plan(&config)),
            ("install", install(&config)),
            ("uninstall", uninstall(&config)),
        ] {
            let windows = windows_report(&reports);
            assert_eq!(
                windows.status,
                ChangeStatus::Conflict,
                "{label} should refuse duplicate managed blocks"
            );
            let message = windows
                .message
                .as_deref()
                .expect("conflict should explain the marker counts");
            assert!(
                message.contains("2 BEGIN marker(s) and 2 END marker(s)"),
                "{label} should count every marker, got: {message}"
            );
        }
        assert_eq!(
            fs::read(&profile).expect("profile should remain readable"),
            before,
            "duplicate blocks must survive untouched instead of being partially removed"
        );
    }

    #[test]
    fn block_state_scans_all_marker_occurrences() {
        assert!(matches!(
            block_state("Set-Alias ll Get-ChildItem\n", BEGIN_MARKER, END_MARKER),
            BlockState::Absent
        ));

        let well_formed = format!("content\n{}\nmore", managed_block("\n"));
        match block_state(&well_formed, BEGIN_MARKER, END_MARKER) {
            BlockState::Present { start, end } => {
                assert_eq!(&well_formed[start..end], managed_block("\n"));
            }
            _ => panic!("a single well-formed block must be present"),
        }

        let reversed = format!("{END_MARKER}\ncontent\n{BEGIN_MARKER}\n");
        match block_state(&reversed, BEGIN_MARKER, END_MARKER) {
            BlockState::Malformed(message) => {
                assert!(
                    message.contains("1 BEGIN marker(s) and 1 END marker(s)")
                        && message.contains("END marker appears before the BEGIN marker"),
                    "unexpected message: {message}"
                );
            }
            _ => panic!("reversed markers must be malformed"),
        }
    }

    #[test]
    fn normalized_eq_treats_crlf_and_lf_as_equal() {
        assert!(normalized_eq("a\r\nb", "a\nb"));
        assert!(normalized_eq("a\nb", "a\nb"));
        assert!(!normalized_eq("a\r\nb", "a\r\n\r\nb"));
        assert!(!normalized_eq("a\rb", "a\nb"));
        assert!(!normalized_eq("a", "ab"));
    }
}
