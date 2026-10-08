use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::TerminalChannel;

pub const INTEGRATION_SCHEMA_VERSION: u32 = 1;
pub const MINIMUM_FRAGMENT_VERSION: &str = "1.21";

/// On-disk application directory under `%LOCALAPPDATA%` for the current
/// Winter-branded installation (config, integration state, backups,
/// dashboard).
pub const APP_DATA_DIR_NAME: &str = "Winter";

/// Case-insensitive ownership key for a filesystem path.
///
/// Windows paths compare case-insensitively, so every ownership record match
/// and target deduplication uses this key instead of byte-wise `PathBuf`
/// equality, which would drop records when `LOCALAPPDATA` casing changes
/// between runs.
#[must_use]
pub(crate) fn path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('/', r"\");
    let trimmed = normalized.strip_prefix(r"\\?\").unwrap_or(&normalized);
    let unescaped = match trimmed.strip_prefix(r"UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => trimmed.to_owned(),
    };
    unescaped.to_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationConfig {
    pub local_app_data: PathBuf,
    pub state_dir: PathBuf,
    /// Known-folder `Documents` root that hosts the PowerShell profile folders.
    pub documents_dir: PathBuf,
}

impl IntegrationConfig {
    #[must_use]
    pub fn new(
        local_app_data: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        documents_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            local_app_data: local_app_data.into(),
            state_dir: state_dir.into(),
            documents_dir: documents_dir.into(),
        }
    }

    pub fn from_environment() -> crate::AppResult<Self> {
        let local_app_data = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            crate::AppError::InvalidConfiguration(
                "LOCALAPPDATA is required for Windows Terminal integration".to_owned(),
            )
        })?;
        let local_app_data = PathBuf::from(local_app_data);
        Ok(Self::new(
            &local_app_data,
            local_app_data.join(APP_DATA_DIR_NAME).join("integration"),
            default_documents_dir(),
        ))
    }

    #[must_use]
    pub fn fragment_path(&self) -> PathBuf {
        self.local_app_data
            .join("Microsoft")
            .join("Windows Terminal")
            .join("Fragments")
            .join(APP_DATA_DIR_NAME)
            .join("actions.json")
    }

    #[must_use]
    pub fn manifest_path(&self) -> PathBuf {
        self.state_dir.join("manifest.json")
    }
}

/// Resolves the per-user `Documents` known folder, falling back to the
/// environment when the Shell API is unavailable.
fn default_documents_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = crate::platform::windows::documents_directory() {
            return path;
        }
    }

    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let profile = PathBuf::from(profile);
        let documents = profile.join("Documents");
        if documents.is_dir() {
            return documents;
        }
        if let Some(onedrive) = std::env::var_os("OneDrive") {
            let onedrive_documents = PathBuf::from(onedrive).join("Documents");
            if onedrive_documents.is_dir() {
                return onedrive_documents;
            }
        }
        return documents;
    }
    PathBuf::from(".")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellKind {
    WindowsPowerShell,
    PowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellIntegrationReport {
    pub shell: ShellKind,
    pub path: PathBuf,
    pub status: ChangeStatus,
    pub backup_path: Option<PathBuf>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettingsTarget {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeStatus {
    Create,
    Update,
    Unchanged,
    Conflict,
    Removed,
    Preserved,
    Missing,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictKind {
    SameIdDifferentBinding,
    SameChordDifferentBinding,
    MalformedKeybinding,
    InvalidSettingsShape,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationConflict {
    pub kind: ConflictKind,
    pub action_id: Option<String>,
    pub keys: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentReport {
    pub path: PathBuf,
    pub status: ChangeStatus,
    pub current_sha256: Option<String>,
    pub desired_sha256: String,
    pub minimum_terminal_version: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetPlan {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
    pub status: ChangeStatus,
    pub existing_binding_count: usize,
    pub bindings_to_add: usize,
    pub managed_binding_count: usize,
    pub conflicts: Vec<IntegrationConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReport {
    pub schema_version: u32,
    pub can_install: bool,
    pub fragment: FragmentReport,
    pub targets: Vec<TargetPlan>,
    pub shell_integration: Vec<ShellIntegrationReport>,
    pub manifest_path: PathBuf,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInstallReport {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
    pub status: ChangeStatus,
    pub added_binding_count: usize,
    pub backup_path: Option<PathBuf>,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub schema_version: u32,
    pub fragment: FragmentReport,
    pub targets: Vec<TargetInstallReport>,
    pub shell_integration: Vec<ShellIntegrationReport>,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetUninstallReport {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
    pub status: ChangeStatus,
    pub removed_binding_count: usize,
    pub preserved_binding_count: usize,
    pub backup_path: Option<PathBuf>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallReport {
    pub schema_version: u32,
    pub fragment_status: ChangeStatus,
    pub targets: Vec<TargetUninstallReport>,
    pub shell_integration: Vec<ShellIntegrationReport>,
    pub manifest_path: PathBuf,
    pub manifest_retained: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorTargetReport {
    pub channel: TerminalChannel,
    pub settings_path: PathBuf,
    pub initialized: bool,
    pub readable: bool,
    pub valid_jsonc: bool,
    pub managed_binding_count: usize,
    pub conflicts: Vec<IntegrationConflict>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub schema_version: u32,
    pub healthy: bool,
    pub fragment: FragmentReport,
    pub targets: Vec<DoctorTargetReport>,
    pub shell_integration: Vec<ShellIntegrationReport>,
    pub manifest_path: PathBuf,
    pub manifest_valid: bool,
    pub issues: Vec<String>,
}
