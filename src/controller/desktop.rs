use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::MouseResizeConfig;
use crate::model::WindowIdentity;
use crate::pane_layout::PaneLayout;
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
            .name("winterminalp-desktop-observer".to_owned())
            .spawn(move || {
                run_observer(
                    worker_value,
                    worker_stopping,
                    worker_error_count,
                    worker_last_error,
                    foreground_poll_interval,
                    mouse_resize,
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

fn run_observer(
    value: Arc<RwLock<DesktopSnapshot>>,
    stopping: Arc<AtomicBool>,
    pane_geometry_errors: Arc<AtomicU64>,
    last_pane_geometry_error: Arc<RwLock<Option<String>>>,
    foreground_poll_interval: Duration,
    mouse_resize: MouseResizeConfig,
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
        thread::sleep(foreground_poll_interval);
    }
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
    use crate::pane_layout::{PaneGeometry, ScreenRect};

    use super::*;

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
