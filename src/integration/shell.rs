//! Managed PowerShell shell integration.
//!
//! Windows Terminal only inherits the working directory of a duplicated pane
//! when the shell reports it through the `OSC 9;9` sequence (WT keeps the
//! pane's `WorkingDirectory` from that sequence and only falls back to the
//! profile's `startingDirectory` — `%USERPROFILE%` by default — when the
//! sequence never arrived). Winter therefore appends a small, reversible
//! prompt wrapper to the PowerShell profiles so that `splitPane` with
//! `splitMode: duplicate` starts in the same directory as the focused pane.
//!
//! PowerShell 7 does not create `Documents\PowerShell` on its own, so a
//! machine where nothing scaffolded a `$PROFILE` has no profile folder for the
//! shell the user actually runs. [`ShellPresence`] decides whether a missing
//! folder means "this shell is installed, create it" or "this shell is absent,
//! leave the machine alone".

use std::path::{Path, PathBuf};

use crate::{AppError, AppResult};

use super::encoding::{UTF16LE_BOM, Utf16LeError, decode_utf16_le, encode_utf16_le_with_bom};
use super::transaction::{atomic_replace, create_backup, read_optional_snapshot};
use super::types::{ChangeStatus, IntegrationConfig, ShellIntegrationReport, ShellKind};

const BEGIN_MARKER: &str = "# >>> Winter shell integration >>>";
const END_MARKER: &str = "# <<< Winter shell integration <<<";

/// Every historical marker pair written by pre-Winter releases, newest
/// branding first; referenced only by the one-shot migration in
/// [`super::legacy`].
///
/// The abandoned project was branded `WinTerminalPP`, while its state
/// directory and fragment were briefly renamed to `WinTerminalP`. Both
/// spellings exist in the wild for the shell markup, so the sweep must know
/// every one of them — see [`uninstall_legacy`].
const LEGACY_SHELL_MARKER_PAIRS: [(&str, &str); 2] = [
    (
        "# >>> WinTerminalPP shell integration >>>",
        "# <<< WinTerminalPP shell integration <<<",
    ),
    (
        "# >>> WinTerminalP shell integration >>>",
        "# <<< WinTerminalP shell integration <<<",
    ),
];

/// Start and end offsets of a located marker block, markers included.
#[derive(Clone, Copy)]
struct MarkerBlock {
    start: usize,
    end: usize,
}

const SNIPPET_BODY: &str = r#"# Managed by Winter. Run `winter uninstall` to remove this block.
if (-not $Global:__Winter_PromptWrapped) {
    $Global:__Winter_PromptWrapped = $true
    $Global:__Winter_OriginalPrompt = $function:prompt
    function global:prompt {
        $__winterLocation = $ExecutionContext.SessionState.Path.CurrentLocation
        # Only a filesystem path is a working directory. Windows Terminal <= 1.25
        # accepts any non-empty report and hands it to CreateProcess, so a PSDrive
        # such as `HKLM:\` would make the next split fail instead of falling back.
        $__winterOsc = if ($__winterLocation.Provider.Name -eq 'FileSystem') {
            "$([char]27)]9;9;`"$($__winterLocation.ProviderPath)`"$([char]7)"
        }
        else {
            $null
        }
        $__winterBase = if ($Global:__Winter_OriginalPrompt) {
            & $Global:__Winter_OriginalPrompt
        }
        else {
            "PS $__winterLocation> "
        }
        if ($__winterBase -is [System.Array]) {
            if ($null -eq $__winterOsc) {
                return $__winterBase
            }
            return @($__winterOsc) + @($__winterBase)
        }
        return [string]$__winterOsc + [string]$__winterBase
    }
}"#;

fn managed_block(newline: &str) -> String {
    let body = SNIPPET_BODY.replace("\r\n", "\n").replace('\n', newline);
    format!("{BEGIN_MARKER}{newline}{body}{newline}{END_MARKER}")
}

/// Extra context on a `Create` plan entry whose shell has no profile folder yet.
const CREATE_FOLDER_MESSAGE: &str = "profile folder will be created";

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

/// Whether the hosts that own the managed profiles are installed.
///
/// PowerShell 7 never creates `Documents\PowerShell` by itself, so that folder
/// is missing on every machine where nothing scaffolded a `$PROFILE` yet.
/// Skipping the target there silently costs working-directory inheritance for
/// every `pwsh` pane, so a missing folder is only treated as "this shell is
/// absent" when its host executable cannot be found either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellPresence {
    windows_powershell: bool,
    powershell: bool,
}

impl ShellPresence {
    /// Probes `PATH` — which carries the Microsoft Store `pwsh.exe` app
    /// execution alias — and then the standard install roots, so an install
    /// started from an environment with a trimmed `PATH` still finds the host.
    fn detect() -> Self {
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map_or_else(Vec::new, |path| std::env::split_paths(&path).collect());
        Self {
            windows_powershell: shell_host_is_installed(ShellKind::WindowsPowerShell, &path_dirs),
            powershell: shell_host_is_installed(ShellKind::PowerShell, &path_dirs),
        }
    }

    const fn is_present(self, shell: ShellKind) -> bool {
        match shell {
            ShellKind::WindowsPowerShell => self.windows_powershell,
            ShellKind::PowerShell => self.powershell,
        }
    }
}

/// File name of the host executable that owns each shell's profile.
const fn host_executable(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::WindowsPowerShell => "powershell.exe",
        ShellKind::PowerShell => "pwsh.exe",
    }
}

fn shell_host_is_installed(shell: ShellKind, path_dirs: &[PathBuf]) -> bool {
    let name = host_executable(shell);
    find_executable(path_dirs.iter().map(PathBuf::as_path), name).is_some()
        || host_candidates(shell)
            .iter()
            .any(|candidate| candidate.is_file())
}

/// First existing `name` below `directories`, mirroring Windows `PATH` lookup.
fn find_executable<'a>(directories: impl Iterator<Item = &'a Path>, name: &str) -> Option<PathBuf> {
    directories
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Well-known host locations used when `PATH` does not carry them: the
/// per-user Store app execution alias and the MSI install roots.
fn host_candidates(shell: ShellKind) -> Vec<PathBuf> {
    let executable = host_executable(shell);
    let mut candidates = Vec::new();
    match shell {
        ShellKind::WindowsPowerShell => {
            if let Some(root) = std::env::var_os("SystemRoot") {
                candidates.push(
                    PathBuf::from(root)
                        .join("System32")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join(executable),
                );
            }
        }
        ShellKind::PowerShell => {
            for variable in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
                if let Some(root) = std::env::var_os(variable) {
                    let root = PathBuf::from(root).join("PowerShell");
                    candidates.push(root.join("7").join(executable));
                    candidates.push(root.join("7-preview").join(executable));
                }
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                candidates.push(
                    PathBuf::from(local)
                        .join("Microsoft")
                        .join("WindowsApps")
                        .join(executable),
                );
            }
        }
    }
    candidates
}

/// Whether a profile path can receive the managed block right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileFolder {
    /// The folder exists; only the profile file decides the next step.
    Present,
    /// The folder is missing, but its shell is installed and the write creates it.
    Creatable,
    /// The folder is missing and its shell is not installed; leave it alone.
    Absent,
}

fn profile_folder(path: &Path, shell: ShellKind, presence: ShellPresence) -> ProfileFolder {
    if path.parent().is_some_and(Path::is_dir) {
        ProfileFolder::Present
    } else if presence.is_present(shell) {
        ProfileFolder::Creatable
    } else {
        ProfileFolder::Absent
    }
}

/// Message for a skipped entry: neither the folder nor the host exists.
fn absent_message(shell: ShellKind) -> String {
    format!(
        "profile folder is not present and no {} was found",
        host_executable(shell)
    )
}

/// Plans both profiles. Per-profile failures are reported on that profile's
/// entry — content conflicts as `Conflict`, environmental I/O failures as
/// `Skipped` — instead of aborting the whole command, so a broken profile
/// never blocks the Terminal action bridge.
pub(crate) fn plan(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    plan_with(config, ShellPresence::detect())
}

pub(crate) fn install(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    install_with(config, ShellPresence::detect())
}

pub(crate) fn uninstall(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| uninstall_profile(config, shell, &path))
        .collect()
}

/// [`plan`] with an explicit shell presence, so tests never depend on the
/// machine that runs them.
fn plan_with(config: &IntegrationConfig, presence: ShellPresence) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| plan_profile(shell, &path, presence))
        .collect()
}

/// [`install`] with an explicit shell presence.
fn install_with(
    config: &IntegrationConfig,
    presence: ShellPresence,
) -> Vec<ShellIntegrationReport> {
    profile_targets(config)
        .into_iter()
        .map(|(shell, path)| install_profile(config, shell, &path, presence))
        .collect()
}

fn plan_profile(shell: ShellKind, path: &Path, presence: ShellPresence) -> ShellIntegrationReport {
    plan_profile_inner(shell, path, presence)
        .unwrap_or_else(|error| shell_error(shell, path, error))
}

fn plan_profile_inner(
    shell: ShellKind,
    path: &Path,
    presence: ShellPresence,
) -> AppResult<ShellIntegrationReport> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(match profile_folder(path, shell, presence) {
            ProfileFolder::Absent => report(
                shell,
                path,
                ChangeStatus::Skipped,
                None,
                Some(absent_message(shell)),
            ),
            ProfileFolder::Present => report(shell, path, ChangeStatus::Create, None, None),
            ProfileFolder::Creatable => report(
                shell,
                path,
                ChangeStatus::Create,
                None,
                Some(CREATE_FOLDER_MESSAGE.to_owned()),
            ),
        });
    };
    let (_, text) = decode_profile(path, &snapshot.bytes)?;
    Ok(match block_state(&text, BEGIN_MARKER, END_MARKER) {
        BlockState::Absent => report(shell, path, ChangeStatus::Update, None, None),
        BlockState::Present(block) => {
            let desired = managed_block(detect_newline(&text));
            if normalized_eq(&text[block.start..block.end], &desired) {
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
    presence: ShellPresence,
) -> ShellIntegrationReport {
    install_profile_inner(config, shell, path, presence)
        .unwrap_or_else(|error| shell_error(shell, path, error))
}

fn install_profile_inner(
    config: &IntegrationConfig,
    shell: ShellKind,
    path: &Path,
    presence: ShellPresence,
) -> AppResult<ShellIntegrationReport> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        if profile_folder(path, shell, presence) == ProfileFolder::Absent {
            return Ok(report(
                shell,
                path,
                ChangeStatus::Skipped,
                None,
                Some(absent_message(shell)),
            ));
        }
        // `atomic_replace` creates a missing parent folder, so an installed
        // shell that never scaffolded `Documents\PowerShell` still receives
        // the wrapper on this first install.
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
        BlockState::Present(block) => {
            let desired = managed_block(newline);
            if normalized_eq(&text[block.start..block.end], &desired) {
                return Ok(report(shell, path, ChangeStatus::Unchanged, None, None));
            }
            let backup = create_backup(&config.state_dir, "shell", path, &snapshot)?;
            let mut updated = String::with_capacity(text.len());
            updated.push_str(&text[..block.start]);
            updated.push_str(&desired);
            updated.push_str(&text[block.end..]);
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

/// Removes every historical block written by pre-Winter releases.
///
/// Both historical spellings are swept. The branding of the abandoned project
/// was `WinTerminalPP`, and only the state directory and fragment were ever
/// renamed to the single-`P` `WinTerminalP`; the shell markers stayed
/// double-`P` on the machines that actually received them. Sweeping only the
/// single-`P` spelling therefore left a live `OSC 9;9` prompt wrapper behind
/// next to the current Winter block — double-wrapped prompts on every new
/// PowerShell session, while `winter doctor` reported a clean migration.
///
/// The legacy markers are namespaced to this project, so a well-formed block
/// is removed even when its content was edited; the current-marker path keeps
/// edited blocks (see [`uninstall_profile_inner`]).
pub(super) fn uninstall_legacy(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
    // A missing profile is reported once per marker pair, and an absent block
    // reports `Missing`, so the pair that actually removes a block is the last
    // report for that profile — which is what owns the meaningful status.
    let mut reports: Vec<ShellIntegrationReport> = Vec::new();
    for (shell, path) in profile_targets(config) {
        for (begin_marker, end_marker) in LEGACY_SHELL_MARKER_PAIRS {
            let report = uninstall_profile_with(
                config,
                shell,
                &path,
                begin_marker,
                end_marker,
                BlockPolicy::RemoveEvenIfEdited,
            )
            .unwrap_or_else(|error| shell_error(shell, &path, error));
            reports.retain(|existing| !(existing.shell == shell && existing.path == report.path));
            reports.push(report);
        }
    }
    reports
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
        BlockState::Present(block) => {
            let newline = detect_newline(&text);
            if policy == BlockPolicy::PreserveEdited
                && !normalized_eq(&text[block.start..block.end], &managed_block(newline))
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
            let mut block_start = block.start;
            if block_start >= newline.len() && text[..block_start].ends_with(newline) {
                block_start -= newline.len();
            }
            let mut block_end = block.end;
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
    Present(MarkerBlock),
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
            BlockState::Present(MarkerBlock {
                start: begin,
                end: end + end_marker.len(),
            })
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

    /// Presence used by the shared test helpers: both hosts count as installed,
    /// so no test depends on what the machine running it has on `PATH`.
    const BOTH_SHELLS: ShellPresence = ShellPresence {
        windows_powershell: true,
        powershell: true,
    };

    /// Presence of a machine that has neither PowerShell host installed.
    const NO_SHELLS: ShellPresence = ShellPresence {
        windows_powershell: false,
        powershell: false,
    };

    /// Shadows the production wrappers for the whole module, so every existing
    /// test keeps its explicit, host-independent shell presence. Tests that
    /// exercise detection use [`plan_with`]/[`install_with`] directly.
    fn plan(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
        plan_with(config, BOTH_SHELLS)
    }

    /// See [`plan`]: the test-module counterpart of [`super::install`].
    fn install(config: &IntegrationConfig) -> Vec<ShellIntegrationReport> {
        install_with(config, BOTH_SHELLS)
    }

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
    fn prompt_wrapper_reports_only_filesystem_locations() {
        let block = managed_block("\n");
        assert!(
            block.contains("]9;9;") && block.contains("ProviderPath"),
            "the wrapper must report the working directory through the provider path"
        );
        assert!(
            block.contains("$__winterLocation.Provider.Name -eq 'FileSystem'"),
            "a PSDrive such as HKLM:\\ must not be reported: Windows Terminal <= 1.25 \
             hands any non-empty report to CreateProcess and the next split then fails"
        );
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
    fn install_skips_a_shell_that_is_not_installed() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        fs::create_dir_all(config.documents_dir.join("WindowsPowerShell"))
            .expect("profile directory should be created");

        let report = install_with(&config, NO_SHELLS);
        let powershell = report
            .iter()
            .find(|entry| entry.shell == ShellKind::PowerShell)
            .expect("powershell target");
        assert_eq!(powershell.status, ChangeStatus::Skipped);
        let message = powershell
            .message
            .as_deref()
            .expect("a skipped shell should explain itself");
        assert!(
            message.contains("profile folder is not present") && message.contains("pwsh.exe"),
            "unexpected message: {message}"
        );
        assert!(
            !config.documents_dir.join("PowerShell").exists(),
            "an absent shell must not get a profile folder"
        );
    }

    #[test]
    fn install_creates_the_profile_folder_of_an_installed_shell() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let presence = ShellPresence {
            windows_powershell: false,
            powershell: true,
        };
        assert!(
            !config.documents_dir.join("PowerShell").exists(),
            "the fixture must start without a PowerShell 7 profile folder"
        );

        let report = install_with(&config, presence);
        let powershell = report
            .iter()
            .find(|entry| entry.shell == ShellKind::PowerShell)
            .expect("powershell target");
        assert_eq!(powershell.status, ChangeStatus::Create);

        let profile = config
            .documents_dir
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1");
        let text = fs::read_to_string(&profile).expect("created profile should be readable");
        assert_eq!(text.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(text.matches(END_MARKER).count(), 1);
        assert!(text.contains("]9;9;"));

        let again = install_with(&config, presence);
        let powershell = again
            .iter()
            .find(|entry| entry.shell == ShellKind::PowerShell)
            .expect("powershell target");
        assert_eq!(
            powershell.status,
            ChangeStatus::Unchanged,
            "a second install must be idempotent after creating the folder"
        );
    }

    #[test]
    fn plan_announces_the_folder_it_will_create() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let config = config_with(temp.path());
        let presence = ShellPresence {
            windows_powershell: true,
            powershell: true,
        };

        let report = plan_with(&config, presence);
        for entry in &report {
            assert_eq!(
                entry.status,
                ChangeStatus::Create,
                "{:?} should be creatable",
                entry.shell
            );
            assert_eq!(entry.message.as_deref(), Some(CREATE_FOLDER_MESSAGE));
        }
        assert!(
            !config.documents_dir.join("PowerShell").exists(),
            "planning must not create anything"
        );

        let absent = plan_with(&config, NO_SHELLS);
        for entry in &absent {
            assert_eq!(entry.status, ChangeStatus::Skipped);
        }
    }

    #[test]
    fn find_executable_returns_the_first_hit_in_path_order() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).expect("first directory should be created");
        fs::create_dir_all(&second).expect("second directory should be created");
        let executable = second.join("pwsh.exe");
        fs::write(&executable, b"stub").expect("stub executable should be written");

        let directories = [first.as_path(), second.as_path()];
        assert_eq!(
            find_executable(directories.into_iter(), "pwsh.exe"),
            Some(executable)
        );
        assert_eq!(
            find_executable(directories.into_iter(), "powershell.exe"),
            None
        );
        assert_eq!(
            find_executable(std::iter::empty(), "pwsh.exe"),
            None,
            "an empty PATH must not panic"
        );
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
            BlockState::Present(block) => {
                assert_eq!(&well_formed[block.start..block.end], managed_block("\n"));
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
