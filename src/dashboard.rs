//! Shared dashboard state exchanged between the running controller and the
//! `winter ui` pane.
//!
//! The controller atomically writes [`DashboardState`] to
//! [`dashboard_path`] whenever the observable state changes; `winter ui`
//! polls that file and renders it. A missing or stale file means the
//! controller is not running.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::AppResult;

/// Schema version written by this build and accepted by `winter ui`.
pub const DASHBOARD_SCHEMA_VERSION: u32 = 1;

/// File name of the dashboard state (sibling of `config.toml`).
pub const DASHBOARD_FILE_NAME: &str = "dashboard.json";

/// File name of the one-shot command channel written by `winter ui` and
/// consumed by the running controller (sibling of `dashboard.json`).
pub const DASHBOARD_COMMAND_FILE_NAME: &str = "command.json";

/// Age beyond which `winter ui` reports the controller as offline.
pub const DASHBOARD_STALE_AFTER_MS: u64 = 2_000;

/// Observable controller state rendered by `winter ui`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardState {
    pub schema_version: u32,
    pub updated_unix_ms: u64,
    pub controller_running: bool,
    pub prefix_armed: bool,
    pub mouse_resize_enabled: bool,
    pub terminal_present: bool,
    /// Unix time of controller start; `0` when unknown (offline frame).
    #[serde(default)]
    pub controller_started_unix_ms: u64,
    /// Whether the low-level input hook handler is still enabled.
    #[serde(default = "default_true")]
    pub hook_active: bool,
    /// Panics caught in the hook handler since controller start.
    #[serde(default)]
    pub hook_panics: u64,
    #[serde(default)]
    pub dispatched_actions: u64,
    #[serde(default)]
    pub failed_actions: u64,
    #[serde(default)]
    pub dropped_actions: u64,
    #[serde(default)]
    pub last_dispatch_error: Option<String>,
    /// Windows Terminal tab list in window order (current WT window only).
    #[serde(default)]
    pub tabs: Vec<DashboardTab>,
    pub panes: Vec<DashboardPane>,
    pub dividers: Vec<DashboardDivider>,
}

fn default_true() -> bool {
    true
}

/// One Windows Terminal tab as exposed by UI Automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardTab {
    /// Zero-based tab index (matches `ActivateTab`-style ordering).
    pub index: u32,
    pub title: String,
    /// The tab Windows Terminal currently shows.
    pub selected: bool,
}

/// One terminal pane rectangle in screen coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardPane {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    /// Pane/tab title as reported by UI Automation (profile name or a title
    /// the user set); empty when unavailable.
    #[serde(default)]
    pub title: String,
    /// Unix time keyboard focus most recently *entered* this pane — stamped
    /// when focus moves onto the pane and left untouched while focus stays
    /// there; `0` = never observed this session. Drives the sidebar activity
    /// column.
    #[serde(default)]
    pub last_focused_unix_ms: u64,
}

/// One pane divider band in screen coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardDivider {
    /// `"vertical"` or `"horizontal"`.
    pub axis: String,
    pub coordinate: i32,
    pub span_start: i32,
    pub span_end: i32,
}

impl DashboardState {
    /// Fresh, empty state for the current instant.
    #[must_use]
    pub fn empty(mouse_resize_enabled: bool) -> Self {
        Self {
            schema_version: DASHBOARD_SCHEMA_VERSION,
            updated_unix_ms: unix_ms_now(),
            controller_running: true,
            prefix_armed: false,
            mouse_resize_enabled,
            terminal_present: false,
            controller_started_unix_ms: 0,
            hook_active: true,
            hook_panics: 0,
            dispatched_actions: 0,
            failed_actions: 0,
            dropped_actions: 0,
            last_dispatch_error: None,
            tabs: Vec::new(),
            panes: Vec::new(),
            dividers: Vec::new(),
        }
    }

    /// Milliseconds since controller start, `None` when the start time is
    /// unknown.
    #[must_use]
    pub fn uptime_ms(&self) -> Option<u64> {
        (self.controller_started_unix_ms > 0)
            .then(|| unix_ms_now().saturating_sub(self.controller_started_unix_ms))
    }

    /// Same state with the timestamp zeroed, for change comparison.
    #[must_use]
    pub fn body(&self) -> Self {
        let mut body = self.clone();
        body.updated_unix_ms = 0;
        body
    }
}

/// Absolute path of the dashboard state file (sibling of `config.toml`).
pub fn dashboard_path() -> AppResult<PathBuf> {
    Ok(crate::config::default_config_path()?.with_file_name(DASHBOARD_FILE_NAME))
}

/// Absolute path of the one-shot command file (sibling of `dashboard.json`).
pub fn command_path() -> AppResult<PathBuf> {
    Ok(crate::config::default_config_path()?.with_file_name(DASHBOARD_COMMAND_FILE_NAME))
}

/// A request from `winter ui` to the running controller.
///
/// The command file is the UI→controller half of the dashboard channel: the
/// UI writes it atomically, the controller's observer takes it (read + delete
/// on every tick, so each command executes at most once) and queues the work
/// onto the action worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardCommand {
    pub schema_version: u32,
    pub requested_unix_ms: u64,
    #[serde(flatten)]
    pub action: DashboardCommandAction,
}

/// Actions the sidebar can request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DashboardCommandAction {
    /// Focus the pane whose rectangle contains this screen point.
    FocusPane { x: i32, y: i32 },
    /// Select the tab with this zero-based index (UI Automation).
    SelectTab { index: u32 },
}

impl DashboardCommand {
    #[must_use]
    pub fn new(action: DashboardCommandAction) -> Self {
        Self {
            schema_version: DASHBOARD_SCHEMA_VERSION,
            requested_unix_ms: unix_ms_now(),
            action,
        }
    }
}

/// Reads and removes the command file; `None` when absent or unparseable.
///
/// The bytes are read first and the file removed before parsing, so a
/// malformed command is consumed exactly once instead of retrying forever.
#[must_use]
pub fn take_command(path: &Path) -> Option<DashboardCommand> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).ok()
}

/// Atomically writes the command file (sibling temp + rename), creating the
/// parent directory when needed. Used by `winter ui`.
pub fn write_command(path: &Path, command: &DashboardCommand) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = {
        let mut os = path.as_os_str().to_os_string();
        os.push(format!(".tmp-{}", std::process::id()));
        PathBuf::from(os)
    };
    std::fs::write(
        &temp,
        serde_json::to_vec(command).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Reads the dashboard state, `None` when missing or unparseable.
#[must_use]
pub fn read_dashboard(path: &Path) -> Option<DashboardState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_command_round_trips_through_the_documented_json_shape() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_COMMAND_FILE_NAME);
        let command = DashboardCommand::new(DashboardCommandAction::FocusPane { x: 12, y: 34 });

        write_command(&path, &command).expect("the UI-side write succeeds");

        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("the command file is readable"),
        )
        .expect("the command file holds valid JSON");
        assert_eq!(raw["schemaVersion"], DASHBOARD_SCHEMA_VERSION);
        assert_eq!(raw["type"], "focusPane");
        assert_eq!(raw["x"], 12);
        assert_eq!(raw["y"], 34);
        assert!(
            raw["requestedUnixMs"].as_u64().is_some(),
            "the flattened timestamp must survive next to the action tag"
        );

        let taken = take_command(&path).expect("the written command parses back");
        assert_eq!(taken, command);
        assert!(!path.exists(), "take_command removes the file it read");
        assert!(
            take_command(&path).is_none(),
            "a second take must find nothing left"
        );
    }

    #[test]
    fn select_tab_command_uses_the_internal_tag_and_round_trips() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_COMMAND_FILE_NAME);
        let command = DashboardCommand::new(DashboardCommandAction::SelectTab { index: 3 });

        write_command(&path, &command).expect("the UI-side write succeeds");

        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("the command file is readable"),
        )
        .expect("the command file holds valid JSON");
        assert_eq!(raw["type"], "selectTab");
        assert_eq!(raw["index"], 3);

        assert_eq!(
            take_command(&path).expect("the written command parses back"),
            command
        );
        assert!(!path.exists());
    }

    /// `take_command` reads before parsing and deletes before returning, so
    /// a malformed payload is consumed exactly once instead of retrying
    /// forever on every observer tick.
    #[test]
    fn malformed_command_file_is_consumed_exactly_once() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_COMMAND_FILE_NAME);
        std::fs::write(&path, b"{ \"schemaVersion\":1, \"type\":").expect("fixture is writable");

        assert!(take_command(&path).is_none());
        assert!(
            !path.exists(),
            "the malformed command must be removed before parsing"
        );
        assert!(take_command(&path).is_none());
    }

    #[test]
    fn take_command_of_an_absent_file_is_a_quiet_none() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        assert!(take_command(&temp.path().join(DASHBOARD_COMMAND_FILE_NAME)).is_none());
    }
}
