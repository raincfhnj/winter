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

/// File name of the dashboard state, sibling of `config.toml`.
pub const DASHBOARD_FILE_NAME: &str = "dashboard.json";

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
    pub panes: Vec<DashboardPane>,
    pub dividers: Vec<DashboardDivider>,
}

fn default_true() -> bool {
    true
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
