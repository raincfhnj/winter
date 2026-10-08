use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::MouseResizeConfig;
use crate::dashboard::{
    DASHBOARD_SCHEMA_VERSION, DashboardDivider, DashboardPane, DashboardState, dashboard_path,
    unix_ms_now,
};
use crate::model::WindowIdentity;
use crate::pane_layout::{PaneLayout, SplitAxis};
use crate::platform::windows::{
    PlatformError, TerminalAccessibility, foreground_hwnd, terminal_window_identity,
};
use crate::{AppError, AppResult};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DesktopSnapshot {
    pub terminal: Option<WindowIdentity>,
    pub pane_layout: PaneLayout,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DesktopCacheReport {
    pub pane_geometry_errors: u64,
    pub last_pane_geometry_error: Option<String>,
}

pub(super) struct DesktopCache {
    value: Arc<RwLock<DesktopSnapshot>>,
    stopping: Arc<AtomicBool>,
    pane_geometry_errors: Arc<AtomicU64>,
    last_pane_geometry_error: Arc<RwLock<Option<String>>>,
    join: Option<JoinHandle<()>>,
}

impl DesktopCache {
    pub(super) fn start(
        foreground_poll_interval: Duration,
        mouse_resize: MouseResizeConfig,
        prefix_armed: Arc<AtomicBool>,
    ) -> AppResult<Self> {
        let value = Arc::new(RwLock::new(DesktopSnapshot::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let pane_geometry_errors = Arc::new(AtomicU64::new(0));
        let last_pane_geometry_error = Arc::new(RwLock::new(None));
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);

        let worker_value = Arc::clone(&value);
        let worker_stopping = Arc::clone(&stopping);
        let worker_error_count = Arc::clone(&pane_geometry_errors);
        let worker_last_error = Arc::clone(&last_pane_geometry_error);
        let join = thread::Builder::new()
            .name("winter-desktop-observer".to_owned())
            .spawn(move || {
                run_observer(
                    worker_value,
                    worker_stopping,
                    worker_error_count,
                    worker_last_error,
                    foreground_poll_interval,
                    mouse_resize,
                    prefix_armed,
                    ready_sender,
                );
            })
            .map_err(|error| {
                AppError::Native(format!("failed to spawn desktop observer thread: {error}"))
            })?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                value,
                stopping,
                pane_geometry_errors,
                last_pane_geometry_error,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(AppError::Platform {
                    source: error,
                    context: " (while starting the pane geometry observer)".to_owned(),
                })
            }
            Err(_) => {
                let _ = join.join();
                Err(AppError::Native(
                    "desktop observer stopped before initialization".to_owned(),
                ))
            }
        }
    }

    pub(super) fn shared(&self) -> Arc<RwLock<DesktopSnapshot>> {
        Arc::clone(&self.value)
    }

    pub(super) fn stop(mut self) -> AppResult<DesktopCacheReport> {
        self.shutdown()?;
        Ok(self.report())
    }

    fn shutdown(&mut self) -> AppResult<()> {
        self.stopping.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| AppError::Native("desktop observer thread panicked".to_owned()))?;
        }
        Ok(())
    }

    fn report(&self) -> DesktopCacheReport {
        DesktopCacheReport {
            pane_geometry_errors: self.pane_geometry_errors.load(Ordering::Relaxed),
            last_pane_geometry_error: read_lock(&self.last_pane_geometry_error).clone(),
        }
    }
}

impl Drop for DesktopCache {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Observer entry point: refreshes the shared snapshot and publishes the
/// dashboard state file on every tick.
///
/// Eight parameters by design — the four shared handles, the cadence, the
/// mouse-resize config, the prefix flag, and the readiness channel — so the
/// observer thread keeps a flat argument list instead of a wrapper struct.
#[allow(clippy::too_many_arguments)]
fn run_observer(
    value: Arc<RwLock<DesktopSnapshot>>,
    stopping: Arc<AtomicBool>,
    pane_geometry_errors: Arc<AtomicU64>,
    last_pane_geometry_error: Arc<RwLock<Option<String>>>,
    foreground_poll_interval: Duration,
    mouse_resize: MouseResizeConfig,
    prefix_armed: Arc<AtomicBool>,
    ready_sender: mpsc::SyncSender<Result<(), PlatformError>>,
) {
    let accessibility = if mouse_resize.enabled {
        match TerminalAccessibility::initialize() {
            Ok(accessibility) => Some(accessibility),
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        }
    } else {
        None
    };

    let mut dashboard = DashboardPublisher::new(prefix_armed, mouse_resize.enabled);
    let mut snapshot = DesktopSnapshot::default();
    let mut resolved_hwnd = 0;
    let mut last_geometry_refresh = Instant::now()
        .checked_sub(mouse_resize.geometry_poll_interval())
        .unwrap_or_else(Instant::now);
    refresh_snapshot(
        &mut snapshot,
        &mut resolved_hwnd,
        &mut last_geometry_refresh,
        accessibility.as_ref(),
        mouse_resize,
        &pane_geometry_errors,
        &last_pane_geometry_error,
    );
    *write_lock(&value) = snapshot.clone();
    // Initial write before the ready signal, so `winter ui` already has data
    // by the time `DesktopCache::start` returns.
    dashboard.publish(&snapshot);
    if ready_sender.send(Ok(())).is_err() {
        return;
    }

    while !stopping.load(Ordering::Acquire) {
        if refresh_snapshot(
            &mut snapshot,
            &mut resolved_hwnd,
            &mut last_geometry_refresh,
            accessibility.as_ref(),
            mouse_resize,
            &pane_geometry_errors,
            &last_pane_geometry_error,
        ) {
            *write_lock(&value) = snapshot.clone();
        }
        dashboard.publish(&snapshot);
        thread::sleep(foreground_poll_interval);
    }
}

/// Writes [`DashboardState`] to the state file from the observer thread.
///
/// The input-hook path only stores the prefix flag; this side reads it once
/// per tick, builds the DTO, and performs the write syscall only when the
/// observable body changed since the last successful write.
struct DashboardPublisher {
    prefix_armed: Arc<AtomicBool>,
    mouse_resize_enabled: bool,
    path: Option<PathBuf>,
    last_written: Option<DashboardState>,
}

impl DashboardPublisher {
    /// Resolves the dashboard path once; an unresolvable path disables
    /// publishing instead of failing the observer.
    fn new(prefix_armed: Arc<AtomicBool>, mouse_resize_enabled: bool) -> Self {
        Self {
            prefix_armed,
            mouse_resize_enabled,
            path: dashboard_path().ok(),
            last_written: None,
        }
    }

    /// Builds and publishes one tick's state; returns whether the file was
    /// rewritten. Checked every tick so a prefix flip lands even when the
    /// desktop snapshot itself is unchanged. Never fails: a write error is
    /// housekeeping, retried on the next tick.
    fn publish(&mut self, snapshot: &DesktopSnapshot) -> bool {
        let prefix_armed = self.prefix_armed.load(Ordering::Relaxed);
        let state = build_dashboard_state(snapshot, prefix_armed, self.mouse_resize_enabled);
        publish_if_changed(self.path.as_deref(), &state, &mut self.last_written)
    }
}

/// Builds the observable dashboard DTO from one desktop snapshot.
///
/// `focused` reads `PaneGeometry::has_keyboard_focus`, which the
/// accessibility adapter populates and this is the first consumer of.
fn build_dashboard_state(
    snapshot: &DesktopSnapshot,
    prefix_armed: bool,
    mouse_resize_enabled: bool,
) -> DashboardState {
    DashboardState {
        schema_version: DASHBOARD_SCHEMA_VERSION,
        updated_unix_ms: unix_ms_now(),
        controller_running: true,
        prefix_armed,
        mouse_resize_enabled,
        terminal_present: snapshot.terminal.is_some(),
        panes: snapshot
            .pane_layout
            .panes()
            .iter()
            .map(|pane| DashboardPane {
                x: pane.bounds.left,
                y: pane.bounds.top,
                width: pane.bounds.width(),
                height: pane.bounds.height(),
                focused: pane.has_keyboard_focus,
            })
            .collect(),
        dividers: snapshot
            .pane_layout
            .dividers()
            .iter()
            .map(|divider| DashboardDivider {
                axis: match divider.axis() {
                    SplitAxis::Vertical => "vertical",
                    SplitAxis::Horizontal => "horizontal",
                }
                .to_owned(),
                coordinate: divider.coordinate(),
                span_start: divider.span_start(),
                span_end: divider.span_end(),
            })
            .collect(),
    }
}

/// Writes `state` only when its [`DashboardState::body`] differs from the
/// last body that was successfully written; the timestamp alone never
/// triggers a write.
///
/// Returns whether the file was rewritten. Only successful writes advance
/// the comparison baseline, so a transient failure is retried on the next
/// tick. `path` of `None` (dashboard path unresolvable) is a quiet no-op.
fn publish_if_changed(
    path: Option<&Path>,
    state: &DashboardState,
    last_written: &mut Option<DashboardState>,
) -> bool {
    let Some(path) = path else {
        return false;
    };
    if last_written
        .as_ref()
        .is_some_and(|previous| previous.body() == state.body())
    {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(state) else {
        return false;
    };
    if write_dashboard(path, &bytes).is_err() {
        return false;
    }
    *last_written = Some(state.clone());
    true
}

/// Housekeeping: atomically replaces the dashboard file by staging the bytes
/// in a sibling `dashboard.json.tmp-<pid>` and renaming it over the target.
///
/// The single observer thread is the only writer, so the pid alone keeps the
/// staging name unique. Every failure — a failed staging write or a locked
/// rename — is cleaned up best-effort and reported only to the caller: the
/// observer drops the error and retries next tick, so a dashboard write can
/// never fail the controller (same contract as backup pruning in
/// `crate::integration::transaction`).
fn write_dashboard(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let temp_path = dashboard_temp_path(path);
    let outcome = std::fs::write(&temp_path, bytes)
        .map_err(|error| AppError::io("write dashboard temporary file", &temp_path, error))
        .and_then(|()| {
            std::fs::rename(&temp_path, path)
                .map_err(|error| AppError::io("replace dashboard state file", path, error))
        });
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    outcome
}

/// Staging path for `path`: the file name plus a `.tmp-<pid>` suffix, so the
/// temp file stays a sibling of the target the atomic rename needs.
fn dashboard_temp_path(path: &Path) -> PathBuf {
    let mut temp_path = path.as_os_str().to_os_string();
    temp_path.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(temp_path)
}

/// Returns true when the snapshot mutated, so the observer only publishes
/// (clones) on real changes instead of every tick.
fn refresh_snapshot(
    snapshot: &mut DesktopSnapshot,
    resolved_hwnd: &mut isize,
    last_geometry_refresh: &mut Instant,
    accessibility: Option<&TerminalAccessibility>,
    mouse_resize: MouseResizeConfig,
    pane_geometry_errors: &AtomicU64,
    last_pane_geometry_error: &RwLock<Option<String>>,
) -> bool {
    let mut changed = false;
    let current_hwnd = foreground_hwnd();
    if current_hwnd != *resolved_hwnd {
        let terminal = if current_hwnd == 0 {
            None
        } else {
            terminal_window_identity(current_hwnd).ok().flatten()
        };
        changed |= apply_terminal_switch(
            snapshot,
            terminal,
            resolved_hwnd,
            current_hwnd,
            last_geometry_refresh,
            mouse_resize.geometry_poll_interval(),
        );
    }

    let Some(accessibility) = accessibility else {
        return changed;
    };
    let Some(terminal) = snapshot.terminal else {
        return changed;
    };
    if last_geometry_refresh.elapsed() < mouse_resize.geometry_poll_interval() {
        return changed;
    }
    *last_geometry_refresh = Instant::now();

    match accessibility.pane_geometries(terminal.hwnd) {
        Ok(panes) => changed |= apply_layout(snapshot, PaneLayout::from_panes(panes)),
        Err(error) => {
            changed |= apply_layout(snapshot, PaneLayout::default());
            pane_geometry_errors.fetch_add(1, Ordering::Relaxed);
            *write_lock(last_pane_geometry_error) = Some(error.to_string());
        }
    }
    changed
}

fn apply_terminal_switch(
    snapshot: &mut DesktopSnapshot,
    terminal: Option<WindowIdentity>,
    resolved_hwnd: &mut isize,
    current_hwnd: isize,
    last_geometry_refresh: &mut Instant,
    geometry_poll_interval: Duration,
) -> bool {
    *resolved_hwnd = current_hwnd;
    *last_geometry_refresh = Instant::now()
        .checked_sub(geometry_poll_interval)
        .unwrap_or_else(Instant::now);

    let mut changed = false;
    if snapshot.terminal != terminal {
        snapshot.terminal = terminal;
        changed = true;
    }
    if snapshot.pane_layout != PaneLayout::default() {
        snapshot.pane_layout = PaneLayout::default();
        changed = true;
    }
    changed
}

fn apply_layout(snapshot: &mut DesktopSnapshot, new_layout: PaneLayout) -> bool {
    if snapshot.pane_layout == new_layout {
        return false;
    }
    snapshot.pane_layout = new_layout;
    true
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use crate::dashboard::DASHBOARD_FILE_NAME;
    use crate::pane_layout::{PaneGeometry, ScreenRect};

    use super::*;

    fn state_for(prefix_armed: bool, updated_unix_ms: u64) -> DashboardState {
        DashboardState {
            schema_version: DASHBOARD_SCHEMA_VERSION,
            updated_unix_ms,
            controller_running: true,
            prefix_armed,
            mouse_resize_enabled: false,
            terminal_present: false,
            panes: Vec::new(),
            dividers: Vec::new(),
        }
    }

    fn staging_leftovers(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .expect("fixture directory stays readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(".tmp-"))
            .collect()
    }

    #[test]
    fn dashboard_state_mirrors_the_snapshot_layout_focus_and_flags() {
        let snapshot = DesktopSnapshot {
            terminal: Some(WindowIdentity {
                hwnd: 11,
                process_id: 3,
                process_started_at_100ns: 4,
                channel: crate::model::TerminalChannel::Stable,
            }),
            pane_layout: PaneLayout::from_panes(vec![
                PaneGeometry {
                    bounds: ScreenRect::new(0, 0, 497, 1000),
                    has_keyboard_focus: false,
                },
                PaneGeometry {
                    bounds: ScreenRect::new(503, 0, 1000, 497),
                    has_keyboard_focus: true,
                },
                PaneGeometry {
                    bounds: ScreenRect::new(503, 503, 1000, 1000),
                    has_keyboard_focus: false,
                },
            ]),
        };

        let state = build_dashboard_state(&snapshot, true, false);

        assert_eq!(state.schema_version, DASHBOARD_SCHEMA_VERSION);
        assert!(state.controller_running);
        assert!(state.prefix_armed);
        assert!(!state.mouse_resize_enabled);
        assert!(state.terminal_present);
        assert_eq!(
            state.panes,
            vec![
                DashboardPane {
                    x: 0,
                    y: 0,
                    width: 497,
                    height: 1000,
                    focused: false,
                },
                DashboardPane {
                    x: 503,
                    y: 0,
                    width: 497,
                    height: 497,
                    focused: true,
                },
                DashboardPane {
                    x: 503,
                    y: 503,
                    width: 497,
                    height: 497,
                    focused: false,
                },
            ]
        );
        assert_eq!(
            state.dividers,
            vec![
                DashboardDivider {
                    axis: "vertical".to_owned(),
                    coordinate: 500,
                    span_start: 0,
                    span_end: 497,
                },
                DashboardDivider {
                    axis: "vertical".to_owned(),
                    coordinate: 500,
                    span_start: 503,
                    span_end: 1000,
                },
                DashboardDivider {
                    axis: "horizontal".to_owned(),
                    coordinate: 500,
                    span_start: 503,
                    span_end: 1000,
                },
            ]
        );

        let absent = build_dashboard_state(&DesktopSnapshot::default(), false, true);
        assert!(!absent.terminal_present);
        assert!(absent.mouse_resize_enabled);
        assert!(!absent.prefix_armed);
        assert!(absent.panes.is_empty());
        assert!(absent.dividers.is_empty());
    }

    #[test]
    fn timestamp_only_changes_do_not_rewrite_the_state_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_FILE_NAME);
        let mut last_written = None;

        assert!(
            publish_if_changed(Some(&path), &state_for(false, 1_000), &mut last_written),
            "the first publish writes the initial state"
        );
        assert!(
            !publish_if_changed(Some(&path), &state_for(false, 2_000), &mut last_written),
            "a fresh timestamp over an identical body must not write"
        );

        let stored = crate::dashboard::read_dashboard(&path).expect("valid dashboard JSON");
        assert_eq!(
            stored.updated_unix_ms, 1_000,
            "the file must still hold the timestamp of the only real write"
        );
        assert!(staging_leftovers(temp.path()).is_empty());
    }

    #[test]
    fn prefix_flip_is_written_even_when_the_snapshot_is_unchanged() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_FILE_NAME);
        let prefix_armed = Arc::new(AtomicBool::new(false));
        let mut publisher = DashboardPublisher {
            prefix_armed: Arc::clone(&prefix_armed),
            mouse_resize_enabled: false,
            path: Some(path.clone()),
            last_written: None,
        };
        let snapshot = DesktopSnapshot::default();

        assert!(
            publisher.publish(&snapshot),
            "the observer starts with an initial write"
        );
        assert!(
            !publisher.publish(&snapshot),
            "an unchanged tick must skip the write"
        );

        prefix_armed.store(true, Ordering::Relaxed);
        assert!(
            publisher.publish(&snapshot),
            "a prefix flip must land even with a frozen snapshot"
        );

        let stored = crate::dashboard::read_dashboard(&path).expect("valid dashboard JSON");
        assert!(stored.prefix_armed);
        assert!(staging_leftovers(temp.path()).is_empty());
    }

    #[test]
    fn publish_without_a_resolved_path_is_a_quiet_no_op() {
        let mut last_written = None;
        assert!(!publish_if_changed(
            None,
            &state_for(false, 1_000),
            &mut last_written
        ));
        assert!(last_written.is_none());
    }

    #[test]
    fn write_dashboard_replaces_the_target_and_leaves_no_staging_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_FILE_NAME);

        let first = state_for(false, 1_000);
        write_dashboard(&path, &serde_json::to_vec(&first).expect("serializable"))
            .expect("the initial write succeeds");
        let stored = crate::dashboard::read_dashboard(&path).expect("parseable by winter ui");
        assert_eq!(stored, first);

        let second = state_for(true, 2_000);
        write_dashboard(&path, &serde_json::to_vec(&second).expect("serializable"))
            .expect("the replacement succeeds");
        let stored = crate::dashboard::read_dashboard(&path).expect("still parseable");
        assert_eq!(stored, second);
        assert!(staging_leftovers(temp.path()).is_empty());
    }

    #[test]
    fn write_dashboard_failure_cleans_up_and_keeps_the_failed_baseline() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_FILE_NAME);
        // A directory can never be replaced by the rename, so the staging
        // file is written and the replace step fails.
        std::fs::create_dir(&path).expect("a directory target blocks the rename");

        assert!(write_dashboard(&path, b"{\"schemaVersion\":1}").is_err());
        assert!(
            staging_leftovers(temp.path()).is_empty(),
            "the staging file must be removed after a failed rename"
        );
        assert!(path.is_dir(), "the pre-existing target is untouched");

        let mut last_written = None;
        assert!(
            !publish_if_changed(Some(&path), &state_for(false, 1_000), &mut last_written),
            "a failed write must not report a publish"
        );
        assert!(
            last_written.is_none(),
            "only a successful write may advance the comparison baseline"
        );
        assert!(staging_leftovers(temp.path()).is_empty());
    }

    #[test]
    fn poisoned_snapshot_locks_remain_recoverable() {
        let lock = Arc::new(RwLock::new(DesktopSnapshot::default()));
        let worker_lock = Arc::clone(&lock);
        let _ = thread::spawn(move || {
            let _guard = worker_lock.write().expect("lock should start healthy");
            panic!("poison fixture");
        })
        .join();

        assert_eq!(*read_lock(&lock), DesktopSnapshot::default());
        *write_lock(&lock) = DesktopSnapshot::default();
    }

    #[test]
    fn terminal_switch_is_dirty_only_when_identity_or_layout_changes() {
        let mut snapshot = DesktopSnapshot::default();
        let mut resolved_hwnd = 7;
        let mut last_geometry_refresh = Instant::now();

        assert!(
            !apply_terminal_switch(
                &mut snapshot,
                None,
                &mut resolved_hwnd,
                7,
                &mut last_geometry_refresh,
                Duration::from_millis(100),
            ),
            "resolving the same hwnd to the same identity must not republish"
        );

        snapshot.terminal = Some(WindowIdentity {
            hwnd: 99,
            process_id: 1,
            process_started_at_100ns: 2,
            channel: crate::model::TerminalChannel::Stable,
        });
        assert!(apply_terminal_switch(
            &mut snapshot,
            None,
            &mut resolved_hwnd,
            8,
            &mut last_geometry_refresh,
            Duration::from_millis(100),
        ));
        assert_eq!(resolved_hwnd, 8);
        assert_eq!(snapshot.terminal, None);
        assert_eq!(snapshot.pane_layout, PaneLayout::default());
    }

    #[test]
    fn layout_is_reassigned_only_when_it_actually_differs() {
        let mut snapshot = DesktopSnapshot::default();
        let layout = PaneLayout::from_panes(vec![PaneGeometry {
            bounds: ScreenRect::new(0, 0, 100, 100),
            has_keyboard_focus: false,
        }]);

        assert!(!apply_layout(&mut snapshot, PaneLayout::default()));
        assert!(apply_layout(&mut snapshot, layout.clone()));
        assert!(!apply_layout(&mut snapshot, layout));
        assert!(apply_layout(&mut snapshot, PaneLayout::default()));
    }

    #[test]
    fn refresh_snapshot_with_disabled_accessibility_stays_quiet() {
        let errors = AtomicU64::new(0);
        let last_error = RwLock::new(None);
        let mut snapshot = DesktopSnapshot::default();
        let mut resolved_hwnd = foreground_hwnd();
        let mut last_geometry_refresh = Instant::now();

        refresh_snapshot(
            &mut snapshot,
            &mut resolved_hwnd,
            &mut last_geometry_refresh,
            None,
            MouseResizeConfig::default(),
            &errors,
            &last_error,
        );
        assert_eq!(errors.load(Ordering::Relaxed), 0);
        assert_eq!(snapshot.pane_layout, PaneLayout::default());
    }
}
