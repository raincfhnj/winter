use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::MouseResizeConfig;
use crate::dashboard::{
    DASHBOARD_SCHEMA_VERSION, DashboardCommand, DashboardCommandAction, DashboardDivider,
    DashboardPane, DashboardState, DashboardTab, command_path, dashboard_path, take_command,
    unix_ms_now,
};
use crate::model::WindowIdentity;
use crate::pane_layout::{PaneGeometry, PaneLayout, ScreenPoint, SplitAxis};
use crate::platform::windows::{
    PlatformError, TabInfo, TerminalAccessibility, foreground_hwnd, terminal_window_identity,
};
use crate::{AppError, AppResult};

use super::DashboardTelemetry;
use super::action_worker::WorkerMessage;

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
        telemetry: DashboardTelemetry,
        worker_sender: mpsc::SyncSender<WorkerMessage>,
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
                    telemetry,
                    worker_sender,
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

/// Observer entry point: refreshes the shared snapshot, the throttled tab
/// list, and the focus-activity map, executes pending dashboard commands, and
/// publishes the dashboard state file on every tick.
///
/// Nine parameters by design — the four shared handles, the cadence, the
/// mouse-resize config, the live telemetry sources, the worker queue the
/// command executor feeds, and the readiness channel — so the observer
/// thread keeps a flat argument list instead of a wrapper struct.
#[allow(clippy::too_many_arguments)]
fn run_observer(
    value: Arc<RwLock<DesktopSnapshot>>,
    stopping: Arc<AtomicBool>,
    pane_geometry_errors: Arc<AtomicU64>,
    last_pane_geometry_error: Arc<RwLock<Option<String>>>,
    foreground_poll_interval: Duration,
    mouse_resize: MouseResizeConfig,
    telemetry: DashboardTelemetry,
    worker_sender: mpsc::SyncSender<WorkerMessage>,
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

    let mut dashboard = DashboardPublisher::new(telemetry, mouse_resize.enabled);
    let mut snapshot = DesktopSnapshot::default();
    let mut resolved_hwnd = 0;
    let mut last_geometry_refresh = Instant::now()
        .checked_sub(mouse_resize.geometry_poll_interval())
        .unwrap_or_else(Instant::now);
    // Dashboard-only observer state: the throttled tab cache, the
    // focus-entry activity map, and the one-shot command file (resolved
    // once, `None` disables the executor instead of failing the observer).
    let mut tabs = TabCache::default();
    let mut activity = FocusActivity::default();
    let command_file = command_path().ok();
    refresh_snapshot(
        &mut snapshot,
        &mut resolved_hwnd,
        &mut last_geometry_refresh,
        accessibility.as_ref(),
        mouse_resize,
        &pane_geometry_errors,
        &last_pane_geometry_error,
    );
    refresh_tabs(accessibility.as_ref(), &snapshot, &mut tabs);
    activity.observe_layout(&snapshot.pane_layout, unix_ms_now());
    *write_lock(&value) = snapshot.clone();
    // Initial write before the ready signal, so `winter ui` already has data
    // by the time `DesktopCache::start` returns.
    dashboard.publish(&snapshot, &tabs.tabs, &activity);
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
        refresh_tabs(accessibility.as_ref(), &snapshot, &mut tabs);
        activity.observe_layout(&snapshot.pane_layout, unix_ms_now());
        if let Some(path) = command_file.as_deref() {
            execute_pending_command(path, snapshot.terminal, &worker_sender);
        }
        dashboard.publish(&snapshot, &tabs.tabs, &activity);
        thread::sleep(foreground_poll_interval);
    }
}

/// Writes [`DashboardState`] to the state file from the observer thread.
///
/// The hot paths (hook dispatcher, action worker, watchdog) only store into
/// the shared telemetry slots; this side reads them once per tick, builds the
/// DTO, and performs the write syscall only when the observable body changed
/// since the last successful write.
struct DashboardPublisher {
    telemetry: DashboardTelemetry,
    mouse_resize_enabled: bool,
    path: Option<PathBuf>,
    last_written: Option<DashboardState>,
}

impl DashboardPublisher {
    /// Resolves the dashboard path once; an unresolvable path disables
    /// publishing instead of failing the observer.
    fn new(telemetry: DashboardTelemetry, mouse_resize_enabled: bool) -> Self {
        Self {
            telemetry,
            mouse_resize_enabled,
            path: dashboard_path().ok(),
            last_written: None,
        }
    }

    /// Builds and publishes one tick's state; returns whether the file was
    /// rewritten. Checked every tick so a counter or flag flip lands even
    /// when the desktop snapshot itself is unchanged. Never fails: a write
    /// error is housekeeping, retried on the next tick.
    fn publish(
        &mut self,
        snapshot: &DesktopSnapshot,
        tabs: &[DashboardTab],
        activity: &FocusActivity,
    ) -> bool {
        let state = build_dashboard_state(
            snapshot,
            &self.telemetry,
            self.mouse_resize_enabled,
            tabs,
            activity,
        );
        publish_if_changed(self.path.as_deref(), &state, &mut self.last_written)
    }
}

/// Builds the observable dashboard DTO from one desktop snapshot plus the
/// live telemetry slots, the observer's tab cache, and the focus-activity
/// map.
///
/// `focused` reads `PaneGeometry::has_keyboard_focus`, which the
/// accessibility adapter populates and this is the first consumer of;
/// `title` is the same adapter's UIA `CurrentName` read; `tabs` is the
/// throttled UIA tab-strip query; `last_focused_unix_ms` comes from the
/// focus-entry map (stable while focus stays on one pane). Every telemetry
/// access is a cheap relaxed atomic load (the last error a short-lived
/// `RwLock` read), so one tick never blocks a hot path.
fn build_dashboard_state(
    snapshot: &DesktopSnapshot,
    telemetry: &DashboardTelemetry,
    mouse_resize_enabled: bool,
    tabs: &[DashboardTab],
    activity: &FocusActivity,
) -> DashboardState {
    DashboardState {
        schema_version: DASHBOARD_SCHEMA_VERSION,
        updated_unix_ms: unix_ms_now(),
        controller_running: true,
        prefix_armed: telemetry.prefix_armed.load(Ordering::Relaxed),
        mouse_resize_enabled,
        terminal_present: snapshot.terminal.is_some(),
        controller_started_unix_ms: telemetry.started_unix_ms,
        hook_active: telemetry.hook_active.load(Ordering::Relaxed),
        hook_panics: telemetry.hook_panics.load(Ordering::Relaxed),
        dispatched_actions: telemetry.dispatched.load(Ordering::Relaxed),
        failed_actions: telemetry.failed.load(Ordering::Relaxed),
        dropped_actions: telemetry.dropped.load(Ordering::Relaxed),
        last_dispatch_error: (*read_lock(&telemetry.last_error)).clone(),
        tabs: tabs.to_vec(),
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
                title: pane.title.clone(),
                last_focused_unix_ms: activity.last_focused(pane_key(pane)),
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

/// Minimum age of the previous tab query before the observer may issue a new
/// one; at the 25 ms default poll interval a throttle window spans ~20 ticks.
const TAB_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Last known tab list plus the two inputs of the query throttle decision
/// (previous query time and the hwnd that list was read from).
///
/// The list survives a failed refresh, so a transient UIA error never blanks
/// an already rendered tab bar.
#[derive(Debug, Default)]
struct TabCache {
    tabs: Vec<DashboardTab>,
    last_query: Option<Instant>,
    last_hwnd: isize,
}

/// Pure throttle decision: query when the terminal window changed, when
/// nothing was ever queried, or when [`TAB_REFRESH_INTERVAL`] elapsed since
/// the previous query.
fn tab_refresh_due(last_query: Option<Instant>, hwnd_changed: bool, now: Instant) -> bool {
    hwnd_changed
        || last_query
            .is_none_or(|queried| now.saturating_duration_since(queried) >= TAB_REFRESH_INTERVAL)
}

/// Refreshes the cached tab list when the throttle allows it.
///
/// The query rides the same UI Automation adapter as the pane geometry
/// reader, so tabs are available exactly when that adapter is (mouse-resize
/// enabled). Nothing is queried while no terminal window is known, and a
/// failed query keeps the last known list (empty on the first failure)
/// instead of failing the dashboard.
fn refresh_tabs(
    accessibility: Option<&TerminalAccessibility>,
    snapshot: &DesktopSnapshot,
    cache: &mut TabCache,
) {
    let (Some(accessibility), Some(terminal)) = (accessibility, snapshot.terminal) else {
        return;
    };
    let now = Instant::now();
    if !tab_refresh_due(cache.last_query, terminal.hwnd != cache.last_hwnd, now) {
        return;
    }
    // The throttle is time-based on purpose: an errored query must not
    // restart the UIA work on the very next 25 ms tick.
    cache.last_query = Some(now);
    cache.last_hwnd = terminal.hwnd;
    if let Ok(tabs) = accessibility.tab_infos(terminal.hwnd) {
        cache.tabs = to_dashboard_tabs(tabs);
    }
}

/// Maps UIA tab records onto the dashboard DTO.
fn to_dashboard_tabs(tabs: Vec<TabInfo>) -> Vec<DashboardTab> {
    tabs.into_iter()
        .map(|tab| DashboardTab {
            index: tab.index,
            title: tab.title,
            selected: tab.selected,
        })
        .collect()
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

/// Identity of one pane across observer ticks: its screen rectangle.
type PaneKey = (i32, i32, i32, i32);

fn pane_key(pane: &PaneGeometry) -> PaneKey {
    (
        pane.bounds.left,
        pane.bounds.top,
        pane.bounds.right,
        pane.bounds.bottom,
    )
}

/// Focus-entry tracker behind [`DashboardPane::last_focused_unix_ms`].
///
/// The map holds, per pane rectangle, the unix time keyboard focus most
/// recently *entered* that pane. The stamp is written only when the focused
/// pane changes between two ticks — never on every tick — so a pane's value
/// stays stable (and the dashboard write policy stays quiet) for as long as
/// focus rests on it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FocusActivity {
    entered_at: HashMap<PaneKey, u64>,
    focused_key: Option<PaneKey>,
}

impl FocusActivity {
    /// Records one observer tick.
    ///
    /// Stale keys — pane rectangles absent from the current pane set — are
    /// pruned so a tab switch cannot leak the old tab's history. An *empty*
    /// pane set means the terminal is momentarily gone (another app is
    /// foreground) or the geometry read failed, not that a new pane set
    /// replaced the old one; pruning nothing in that case keeps history
    /// across such blips.
    fn observe(&mut self, focused_key: Option<PaneKey>, current_keys: &HashSet<PaneKey>, now: u64) {
        if !current_keys.is_empty() {
            self.entered_at.retain(|key, _| current_keys.contains(key));
        }
        if focused_key != self.focused_key {
            self.focused_key = focused_key;
            if let Some(key) = focused_key {
                self.entered_at.insert(key, now);
            }
        }
    }

    /// Convenience wrapper over [`Self::observe`] deriving the focused key
    /// and the current key set from one pane layout.
    fn observe_layout(&mut self, layout: &PaneLayout, now: u64) {
        let panes = layout.panes();
        let focused_key = panes
            .iter()
            .find(|pane| pane.has_keyboard_focus)
            .map(pane_key);
        let current_keys = panes.iter().map(pane_key).collect::<HashSet<_>>();
        self.observe(focused_key, &current_keys, now);
    }

    /// Recorded focus-entry time for one pane; `0` when never observed.
    fn last_focused(&self, key: PaneKey) -> u64 {
        self.entered_at.get(&key).copied().unwrap_or(0)
    }
}

/// Maps a dashboard command onto the worker message that executes it.
///
/// Returns `None` when no terminal window is currently known: the command
/// file has already been consumed at that point, so there is no target to
/// dispatch against and the command is dropped.
fn command_to_message(
    command: DashboardCommand,
    target: Option<WindowIdentity>,
) -> Option<WorkerMessage> {
    let target = target?;
    Some(match command.action {
        DashboardCommandAction::FocusPane { x, y } => WorkerMessage::FocusAtPoint {
            target,
            point: ScreenPoint::new(x, y),
        },
        DashboardCommandAction::SelectTab { index } => WorkerMessage::SelectTab { target, index },
    })
}

/// Consumes at most one pending dashboard command per observer tick and
/// queues it on the action worker.
///
/// A missing file (the common case) is just a failed read inside
/// `take_command`. A command that arrives while no terminal window is known
/// is dropped silently — the file is already consumed, and re-queueing a
/// targetless command could never succeed. A saturated worker queue drops
/// the message rather than blocking the observer; execution failures are
/// recorded by the worker into the shared telemetry.
fn execute_pending_command(
    path: &Path,
    target: Option<WindowIdentity>,
    worker_sender: &mpsc::SyncSender<WorkerMessage>,
) {
    let Some(command) = take_command(path) else {
        return;
    };
    let Some(message) = command_to_message(command, target) else {
        return;
    };
    let _ = worker_sender.try_send(message);
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
    use crate::dashboard::{DASHBOARD_COMMAND_FILE_NAME, DASHBOARD_FILE_NAME, write_command};
    use crate::pane_layout::{PaneGeometry, ScreenRect};

    use super::*;

    fn state_for(prefix_armed: bool, updated_unix_ms: u64) -> DashboardState {
        DashboardState {
            updated_unix_ms,
            prefix_armed,
            ..DashboardState::empty(false)
        }
    }

    fn telemetry(prefix_armed: bool) -> DashboardTelemetry {
        DashboardTelemetry {
            prefix_armed: Arc::new(AtomicBool::new(prefix_armed)),
            ..DashboardTelemetry::default()
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
                    title: String::new(),
                },
                PaneGeometry {
                    bounds: ScreenRect::new(503, 0, 1000, 497),
                    has_keyboard_focus: true,
                    title: String::new(),
                },
                PaneGeometry {
                    bounds: ScreenRect::new(503, 503, 1000, 1000),
                    has_keyboard_focus: false,
                    title: String::new(),
                },
            ]),
        };

        let state = build_dashboard_state(
            &snapshot,
            &telemetry(true),
            false,
            &[],
            &FocusActivity::default(),
        );

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
                    title: String::new(),
                    last_focused_unix_ms: 0,
                },
                DashboardPane {
                    x: 503,
                    y: 0,
                    width: 497,
                    height: 497,
                    focused: true,
                    title: String::new(),
                    last_focused_unix_ms: 0,
                },
                DashboardPane {
                    x: 503,
                    y: 503,
                    width: 497,
                    height: 497,
                    focused: false,
                    title: String::new(),
                    last_focused_unix_ms: 0,
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

        let absent = build_dashboard_state(
            &DesktopSnapshot::default(),
            &DashboardTelemetry::default(),
            true,
            &[],
            &FocusActivity::default(),
        );
        assert!(!absent.terminal_present);
        assert!(absent.mouse_resize_enabled);
        assert!(!absent.prefix_armed);
        assert!(absent.panes.is_empty());
        assert!(absent.dividers.is_empty());
    }

    /// Every frozen telemetry field of `DashboardState` must be sourced from
    /// the shared slots, including the pane titles the accessibility adapter
    /// read via UIA `CurrentName`.
    #[test]
    fn dashboard_state_maps_titles_and_all_telemetry_counters() {
        let started = 1_700_000_000_000;
        let telemetry = DashboardTelemetry {
            started_unix_ms: started,
            hook_active: Arc::new(AtomicBool::new(false)),
            hook_panics: Arc::new(AtomicU64::new(3)),
            dispatched: Arc::new(AtomicU64::new(11)),
            failed: Arc::new(AtomicU64::new(4)),
            dropped: Arc::new(AtomicU64::new(2)),
            last_error: Arc::new(RwLock::new(Some("injection refused".to_owned()))),
            ..DashboardTelemetry::default()
        };
        let snapshot = DesktopSnapshot {
            terminal: Some(WindowIdentity {
                hwnd: 21,
                process_id: 5,
                process_started_at_100ns: 6,
                channel: crate::model::TerminalChannel::Stable,
            }),
            pane_layout: PaneLayout::from_panes(vec![PaneGeometry {
                bounds: ScreenRect::new(0, 0, 497, 800),
                has_keyboard_focus: false,
                title: "build: main".to_owned(),
            }]),
        };

        let state =
            build_dashboard_state(&snapshot, &telemetry, true, &[], &FocusActivity::default());

        assert_eq!(state.controller_started_unix_ms, started);
        assert!(
            state.uptime_ms().is_some(),
            "a known start time must yield a computable uptime"
        );
        assert!(!state.hook_active);
        assert_eq!(state.hook_panics, 3);
        assert_eq!(state.dispatched_actions, 11);
        assert_eq!(state.failed_actions, 4);
        assert_eq!(state.dropped_actions, 2);
        assert_eq!(
            state.last_dispatch_error.as_deref(),
            Some("injection refused")
        );
        assert_eq!(state.panes.len(), 1);
        assert_eq!(state.panes[0].title, "build: main");
        assert!(state.mouse_resize_enabled);
        assert!(state.terminal_present);
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
        let telemetry = telemetry(false);
        let publisher_telemetry = telemetry.clone();
        let mut publisher = DashboardPublisher {
            telemetry: publisher_telemetry,
            mouse_resize_enabled: false,
            path: Some(path.clone()),
            last_written: None,
        };
        let snapshot = DesktopSnapshot::default();

        assert!(
            publisher.publish(&snapshot, &[], &FocusActivity::default()),
            "the observer starts with an initial write"
        );
        assert!(
            !publisher.publish(&snapshot, &[], &FocusActivity::default()),
            "an unchanged tick must skip the write"
        );

        telemetry.prefix_armed.store(true, Ordering::Relaxed);
        assert!(
            publisher.publish(&snapshot, &[], &FocusActivity::default()),
            "a prefix flip must land even with a frozen snapshot"
        );

        let stored = crate::dashboard::read_dashboard(&path).expect("valid dashboard JSON");
        assert!(stored.prefix_armed);
        assert!(staging_leftovers(temp.path()).is_empty());
    }

    /// A live counter bump (as produced by a resize drag) changes the body,
    /// so the body-compared write fires on the very next tick.
    #[test]
    fn counter_bumps_are_written_even_when_the_snapshot_is_unchanged() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_FILE_NAME);
        let telemetry = telemetry(false);
        let publisher_telemetry = telemetry.clone();
        let mut publisher = DashboardPublisher {
            telemetry: publisher_telemetry,
            mouse_resize_enabled: false,
            path: Some(path.clone()),
            last_written: None,
        };
        let snapshot = DesktopSnapshot::default();

        assert!(publisher.publish(&snapshot, &[], &FocusActivity::default()));
        assert!(!publisher.publish(&snapshot, &[], &FocusActivity::default()));

        telemetry.dispatched.store(7, Ordering::Relaxed);
        assert!(
            publisher.publish(&snapshot, &[], &FocusActivity::default()),
            "a dispatch counter bump must rewrite the state file"
        );

        let stored = crate::dashboard::read_dashboard(&path).expect("valid dashboard JSON");
        assert_eq!(stored.dispatched_actions, 7);
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
            title: String::new(),
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

    /// Focus-entry semantics: the stamp is written only when focus *moves*
    /// onto a pane, never refreshed while focus rests on it.
    #[test]
    fn focus_activity_stamps_on_entry_only_and_prunes_stale_keys() {
        let pane_a: PaneKey = (0, 0, 500, 500);
        let pane_b: PaneKey = (500, 0, 1000, 500);
        let pane_c: PaneKey = (0, 500, 500, 1_000);
        let current = HashSet::from([pane_a, pane_b]);
        let mut activity = FocusActivity::default();

        activity.observe(Some(pane_a), &current, 1_000);
        assert_eq!(activity.last_focused(pane_a), 1_000);

        activity.observe(Some(pane_a), &current, 2_000);
        assert_eq!(
            activity.last_focused(pane_a),
            1_000,
            "focus resting on the same pane must not restamp it"
        );

        activity.observe(Some(pane_b), &current, 3_000);
        assert_eq!(activity.last_focused(pane_b), 3_000);
        assert_eq!(activity.last_focused(pane_a), 1_000);

        activity.observe(Some(pane_a), &current, 4_000);
        assert_eq!(
            activity.last_focused(pane_a),
            4_000,
            "focus returning to a pane is a fresh entry"
        );

        let replacement = HashSet::from([pane_b, pane_c]);
        activity.observe(Some(pane_c), &replacement, 5_000);
        assert_eq!(activity.last_focused(pane_c), 5_000);
        assert_eq!(
            activity.last_focused(pane_a),
            0,
            "a key absent from the replacement pane set is pruned"
        );

        // An empty pane set means the terminal is momentarily gone (or the
        // geometry read failed), not that a new pane set replaced the old
        // one: history must survive the blip.
        activity.observe(None, &HashSet::new(), 6_000);
        assert_eq!(activity.last_focused(pane_b), 3_000);
        activity.observe(Some(pane_b), &replacement, 7_000);
        assert_eq!(
            activity.last_focused(pane_b),
            7_000,
            "focus re-entering after the blip stamps anew"
        );
        assert_eq!(activity.last_focused(pane_c), 5_000);
    }

    #[test]
    fn tab_refresh_is_throttled_by_interval_and_woken_by_a_new_window() {
        let base = Instant::now();
        assert!(
            tab_refresh_due(None, false, base),
            "the first query is always due"
        );
        assert!(
            !tab_refresh_due(Some(base), false, base + Duration::from_millis(499)),
            "a query inside the throttle interval must wait"
        );
        assert!(
            tab_refresh_due(Some(base), false, base + TAB_REFRESH_INTERVAL),
            "an elapsed interval re-arms the query"
        );
        assert!(
            tab_refresh_due(Some(base), true, base + Duration::from_millis(1)),
            "a terminal hwnd change bypasses the interval"
        );
    }

    #[test]
    fn dashboard_commands_map_onto_worker_messages_only_with_a_terminal() {
        let target = WindowIdentity {
            hwnd: 42,
            process_id: 7,
            process_started_at_100ns: 9,
            channel: crate::model::TerminalChannel::Stable,
        };
        let focus = DashboardCommand::new(DashboardCommandAction::FocusPane { x: 10, y: 20 });
        let select = DashboardCommand::new(DashboardCommandAction::SelectTab { index: 3 });

        let message = command_to_message(focus.clone(), Some(target))
            .expect("a focus command maps once a terminal is known");
        assert!(
            matches!(
                message,
                WorkerMessage::FocusAtPoint { target: got, point }
                    if got == target && point == ScreenPoint::new(10, 20)
            ),
            "focusPane must become FocusAtPoint with the same screen point"
        );

        let message = command_to_message(select.clone(), Some(target))
            .expect("a tab command maps once a terminal is known");
        assert!(
            matches!(
                message,
                WorkerMessage::SelectTab {
                    target: got,
                    index: 3
                } if got == target
            ),
            "selectTab must become SelectTab with the same index"
        );

        assert!(
            command_to_message(focus, None).is_none(),
            "without a terminal there is no dispatch target"
        );
        assert!(command_to_message(select, None).is_none());
    }

    /// The command file is consumed even when nothing can execute it, so the
    /// executor must drop a targetless or unqueueable command instead of
    /// replaying it or blocking the observer.
    #[test]
    fn execute_pending_command_consumes_the_file_and_never_blocks() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join(DASHBOARD_COMMAND_FILE_NAME);
        let (sender, receiver) = mpsc::sync_channel(1);
        let command = DashboardCommand::new(DashboardCommandAction::SelectTab { index: 1 });

        write_command(&path, &command).expect("the command fixture is written");
        execute_pending_command(&path, None, &sender);
        assert!(
            !path.exists(),
            "a targetless command is still consumed from disk"
        );
        assert!(
            receiver.try_recv().is_err(),
            "no message may be queued without a terminal"
        );

        write_command(&path, &command).expect("rewrite for the target case");
        let target = WindowIdentity {
            hwnd: 42,
            process_id: 7,
            process_started_at_100ns: 9,
            channel: crate::model::TerminalChannel::Stable,
        };
        execute_pending_command(&path, Some(target), &sender);
        assert!(
            matches!(
                receiver.try_recv().expect("the message is queued"),
                WorkerMessage::SelectTab { index: 1, .. }
            ),
            "a target-backed command reaches the worker queue"
        );

        // The queue is empty again; fill its single slot, then prove a second
        // command is dropped (not retried, not blocking) while still being
        // consumed from disk.
        write_command(&path, &command).expect("rewrite for the saturated case");
        sender
            .send(WorkerMessage::Stop)
            .expect("the queue has room for one message");
        execute_pending_command(&path, Some(target), &sender);
        assert!(!path.exists(), "a dropped command is still consumed");
        assert!(
            receiver.try_recv().is_ok(),
            "the pre-existing message stays queued"
        );
        assert!(
            receiver.try_recv().is_err(),
            "the saturated send is dropped instead of blocking the observer"
        );
    }

    #[test]
    fn dashboard_state_carries_tabs_and_focus_entry_times() {
        let snapshot = DesktopSnapshot {
            terminal: Some(WindowIdentity {
                hwnd: 31,
                process_id: 8,
                process_started_at_100ns: 9,
                channel: crate::model::TerminalChannel::Stable,
            }),
            pane_layout: PaneLayout::from_panes(vec![
                PaneGeometry {
                    bounds: ScreenRect::new(0, 0, 497, 800),
                    has_keyboard_focus: false,
                    title: String::new(),
                },
                PaneGeometry {
                    bounds: ScreenRect::new(503, 0, 1_000, 800),
                    has_keyboard_focus: true,
                    title: String::new(),
                },
            ]),
        };
        let tabs = vec![
            DashboardTab {
                index: 0,
                title: "build".to_owned(),
                selected: false,
            },
            DashboardTab {
                index: 1,
                title: "logs".to_owned(),
                selected: true,
            },
        ];
        let mut activity = FocusActivity::default();
        activity.observe_layout(&snapshot.pane_layout, 1_700_000_000_000);

        let state = build_dashboard_state(&snapshot, &telemetry(false), false, &tabs, &activity);

        assert_eq!(state.tabs, tabs);
        assert_eq!(state.panes.len(), 2);
        assert_eq!(state.panes[0].last_focused_unix_ms, 0, "never observed");
        assert_eq!(
            state.panes[1].last_focused_unix_ms, 1_700_000_000_000,
            "the focused pane carries its focus-entry time"
        );
    }

    /// Steady state: focus rests on the same pane across ticks, so the body
    /// (everything except the timestamp) must be identical — otherwise the
    /// body-compared write policy would rewrite the file every 25 ms.
    #[test]
    fn unchanged_focus_produces_identical_bodies_between_builds() {
        let snapshot = DesktopSnapshot {
            terminal: Some(WindowIdentity {
                hwnd: 31,
                process_id: 8,
                process_started_at_100ns: 9,
                channel: crate::model::TerminalChannel::Stable,
            }),
            pane_layout: PaneLayout::from_panes(vec![PaneGeometry {
                bounds: ScreenRect::new(0, 0, 497, 800),
                has_keyboard_focus: true,
                title: String::new(),
            }]),
        };
        let tabs = vec![DashboardTab {
            index: 0,
            title: "build".to_owned(),
            selected: true,
        }];
        let telemetry = telemetry(false);
        let mut activity = FocusActivity::default();

        activity.observe_layout(&snapshot.pane_layout, 1_000);
        let first = build_dashboard_state(&snapshot, &telemetry, false, &tabs, &activity);
        // A later tick, same focus: the entry stamp must not move.
        activity.observe_layout(&snapshot.pane_layout, 2_000);
        let second = build_dashboard_state(&snapshot, &telemetry, false, &tabs, &activity);

        assert_eq!(first.panes[0].last_focused_unix_ms, 1_000);
        assert_eq!(
            first.body(),
            second.body(),
            "two builds with unchanged focus must produce equal bodies"
        );
    }
}
