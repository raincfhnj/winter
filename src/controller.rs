use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::integration::{ChangeStatus, DoctorReport};
use crate::keymap::managed_bindings;
use crate::{AppError, AppResult, ControllerConfig};

#[cfg(target_os = "windows")]
mod action_worker;
#[cfg(target_os = "windows")]
mod desktop;
#[cfg(target_os = "windows")]
mod keyboard;
#[cfg(target_os = "windows")]
mod pointer;

const DEFAULT_FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DEFAULT_ACTION_QUEUE_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerOptions {
    /// Whether the native Windows Terminal should be opened before listening.
    pub launch_terminal: bool,
    /// Set only after integration diagnostics confirm that the action bridge is installed.
    pub bridge_ready: bool,
    pub foreground_poll_interval: Duration,
    pub action_queue_capacity: usize,
}

impl Default for ControllerOptions {
    fn default() -> Self {
        Self {
            launch_terminal: false,
            bridge_ready: false,
            foreground_poll_interval: DEFAULT_FOREGROUND_POLL_INTERVAL,
            action_queue_capacity: DEFAULT_ACTION_QUEUE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerRunReport {
    pub dispatched_actions: u64,
    pub failed_actions: u64,
    pub dropped_actions: u64,
    pub last_dispatch_error: Option<String>,
    pub pane_geometry_errors: u64,
    pub last_pane_geometry_error: Option<String>,
    /// Whether the process runs with per-monitor DPI awareness enabled, which
    /// low-level mouse hook coordinates and UI Automation rectangles depend on.
    pub dpi_awareness_active: bool,
}

/// Returns true only when every discovered Terminal channel has the complete,
/// conflict-free action bridge required before the hook may consume Prefix keys.
///
/// The expected count is the registry-derived [`managed_bindings`] table, and
/// the per-target `managed_binding_count` counts installed `(id, chord)` pairs,
/// so readiness means every canonical action id and chord is present.
#[must_use]
pub fn bridge_is_ready(report: &DoctorReport) -> bool {
    let expected_bindings = managed_bindings().len();
    report.healthy
        && report.fragment.status == ChangeStatus::Unchanged
        && !report.targets.is_empty()
        && report.targets.iter().all(|target| {
            target.initialized
                && target.readable
                && target.valid_jsonc
                && target.conflicts.is_empty()
                && target.managed_binding_count == expected_bindings
        })
}

#[cfg(target_os = "windows")]
mod implementation {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    use crate::model::{TerminalAction, WindowIdentity};
    use crate::pane_layout::{PaneDivider, ScreenPoint, SplitAxis};
    use crate::platform::windows::{
        HookDecision, InputHook, MouseEventKind, PlatformError, RawInputEvent, RawKeyEvent,
        RawMouseEvent, SingleInstanceGuard, enable_per_monitor_dpi_awareness, foreground_hwnd,
        is_current_process_elevated, launch_windows_terminal, show_pane_resize_cursor,
    };
    use crate::prefix::{
        CancelReason, KeyChord, KeyDisposition, KeyTransition, PhysicalKey, PrefixCommand,
        PrefixMachine,
    };

    use super::action_worker::{ActionWorker, WorkerMessage};
    use super::desktop::{DesktopCache, DesktopSnapshot};
    use super::keyboard::KeyboardNormalizer;
    use super::pointer::PointerDragState;
    use super::{AppError, AppResult, ControllerConfig, ControllerOptions, ControllerRunReport};

    const MIN_FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(5);
    const MAX_FOREGROUND_POLL_INTERVAL: Duration = Duration::from_secs(1);
    const MAX_ACTION_QUEUE_CAPACITY: usize = 1_024;
    const SHUTDOWN_HANDSHAKE_WINDOW: Duration = Duration::from_millis(3_000);
    const SHUTDOWN_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(200);

    pub(super) fn run(
        config: &ControllerConfig,
        options: ControllerOptions,
    ) -> AppResult<ControllerRunReport> {
        let dpi_active = enable_per_monitor_dpi_awareness();
        validate_options(options)?;
        if !is_current_process_elevated().map_err(map_platform_error)? {
            return Err(AppError::ControllerRequiresElevation);
        }
        if !options.bridge_ready {
            return Err(AppError::InvalidConfiguration(
                "Windows Terminal action bridge is not ready; run `winter doctor` and `winter install` first"
                    .to_owned(),
            ));
        }

        let _single_instance = SingleInstanceGuard::acquire().map_err(map_platform_error)?;
        if options.launch_terminal {
            let _child = launch_windows_terminal().map_err(map_platform_error)?;
        }

        let desktop_cache =
            DesktopCache::start(options.foreground_poll_interval, config.mouse_resize)?;
        let cached_desktop = desktop_cache.shared();
        let dropped_actions = Arc::new(AtomicU64::new(0));

        let action_worker = ActionWorker::start(options.action_queue_capacity)?;
        let (shutdown_sender, shutdown_receiver) = mpsc::sync_channel(1);

        let prefix_runtime = config.prefix_config()?;
        let prefix_chord = prefix_runtime.prefix_chord;
        let mouse_resize_enabled = config.mouse_resize.enabled;
        let mut dispatcher = HookDispatcher {
            prefix: PrefixMachine::new(prefix_runtime),
            prefix_chord,
            normalizer: KeyboardNormalizer::default(),
            pending_shutdown_key: None,
            pointer_drag: PointerDragState::default(),
            worker_sender: action_worker.sender(),
            shutdown_sender,
            dropped_actions_for_hook: Arc::clone(&dropped_actions),
            divider_hit_slop_px: i32::from(config.mouse_resize.divider_hit_slop_px),
            snapshots: DesktopSnapshotSource::new(cached_desktop),
            last_cursor_axis: None,
        };
        let hook = InputHook::start(
            Box::new(move |raw_event| match raw_event {
                RawInputEvent::Keyboard(raw_event) => dispatcher.on_keyboard(raw_event),
                RawInputEvent::Mouse(raw_event) => dispatcher.on_mouse(raw_event),
            }),
            mouse_resize_enabled,
        );
        let hook = match hook {
            Ok(hook) => hook,
            Err(error) => {
                let worker_result = action_worker.stop();
                let mut app_error = map_platform_error(error);
                if let AppError::Platform { context, .. } = &mut app_error {
                    match worker_result {
                        Ok(report) if report.failed_actions > 0 => {
                            context.push_str(&format!(
                                "; action worker also recorded {} failed action(s), last error: {}",
                                report.failed_actions,
                                report.last_dispatch_error.as_deref().unwrap_or("unknown")
                            ));
                        }
                        Err(worker_error) => {
                            context.push_str(&format!("; {worker_error}"));
                        }
                        _ => {}
                    }
                }
                return Err(app_error);
            }
        };

        let shutdown_result = wait_for_shutdown(&shutdown_receiver, &hook);

        let hook_result = hook.stop().map_err(map_platform_error);
        let cache_result = desktop_cache.stop();
        let worker_result = action_worker.stop();

        shutdown_result?;
        hook_result?;
        let cache_report = cache_result?;
        let report = worker_result?;

        Ok(ControllerRunReport {
            dispatched_actions: report.dispatched_actions,
            failed_actions: report.failed_actions,
            dropped_actions: dropped_actions.load(Ordering::Relaxed),
            last_dispatch_error: report.last_dispatch_error,
            pane_geometry_errors: cache_report.pane_geometry_errors,
            last_pane_geometry_error: cache_report.last_pane_geometry_error,
            dpi_awareness_active: dpi_active,
        })
    }

    /// Waits for the shutdown handshake while polling hook health, so a
    /// disabled handler (panic) or a silently removed hook cannot leave the
    /// controller parked in a blocking receive forever.
    fn wait_for_shutdown(receiver: &mpsc::Receiver<()>, hook: &InputHook) -> AppResult<()> {
        loop {
            match receiver.recv_timeout(SHUTDOWN_WATCHDOG_POLL_INTERVAL) {
                Ok(()) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let panics = hook.handler_panic_count();
                    if !hook.is_handler_active() || panics > 0 {
                        return Err(AppError::Native(format!(
                            "input hook handler was disabled (panics: {panics})"
                        )));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Native(
                        "controller shutdown channel disconnected unexpectedly".to_owned(),
                    ));
                }
            }
        }
    }

    fn validate_options(options: ControllerOptions) -> AppResult<()> {
        if !(MIN_FOREGROUND_POLL_INTERVAL..=MAX_FOREGROUND_POLL_INTERVAL)
            .contains(&options.foreground_poll_interval)
        {
            return Err(AppError::InvalidConfiguration(format!(
                "foreground poll interval must be between {} and {} milliseconds",
                MIN_FOREGROUND_POLL_INTERVAL.as_millis(),
                MAX_FOREGROUND_POLL_INTERVAL.as_millis()
            )));
        }
        if !(1..=MAX_ACTION_QUEUE_CAPACITY).contains(&options.action_queue_capacity) {
            return Err(AppError::InvalidConfiguration(format!(
                "action queue capacity must be between 1 and {MAX_ACTION_QUEUE_CAPACITY}"
            )));
        }
        Ok(())
    }

    fn map_platform_error(error: PlatformError) -> AppError {
        if matches!(&error, PlatformError::AlreadyRunning) {
            AppError::ControllerAlreadyRunning
        } else {
            AppError::platform(error)
        }
    }

    const fn hook_decision(consumes: bool) -> HookDecision {
        if consumes {
            HookDecision::Consume
        } else {
            HookDecision::Pass
        }
    }

    /// Returns true only for the armed key's key-up within
    /// [`SHUTDOWN_HANDSHAKE_WINDOW`]. An event for a different physical key
    /// invalidates the handshake, and an expired handshake is dropped without
    /// firing, so a missed key-up can never shut the controller down later.
    fn shutdown_handshake_should_fire<K: Copy + PartialEq>(
        pending: &mut Option<(K, Instant)>,
        event_key: K,
        is_up: bool,
        now: Instant,
    ) -> bool {
        let Some((armed_key, armed_at)) = *pending else {
            return false;
        };
        if event_key != armed_key {
            *pending = None;
            return false;
        }
        if !is_up {
            return false;
        }
        if now.saturating_duration_since(armed_at) > SHUTDOWN_HANDSHAKE_WINDOW {
            *pending = None;
            return false;
        }
        *pending = None;
        true
    }

    /// Desktop snapshot reader for the hook thread.
    ///
    /// Lock contention or poisoning falls back to the last successfully read
    /// snapshot, so `None` means the state was genuinely never known instead of
    /// the observer lock being briefly busy.
    struct DesktopSnapshotSource {
        shared: Arc<RwLock<DesktopSnapshot>>,
        last_known: Mutex<Option<DesktopSnapshot>>,
    }

    impl DesktopSnapshotSource {
        fn new(shared: Arc<RwLock<DesktopSnapshot>>) -> Self {
            Self {
                shared,
                last_known: Mutex::new(None),
            }
        }

        fn snapshot(&self) -> Option<DesktopSnapshot> {
            let mut last_known = self.lock_last_known();
            match self.shared.try_read() {
                Ok(guard) => {
                    *last_known = Some((*guard).clone());
                    last_known.clone()
                }
                Err(_) => last_known.clone(),
            }
        }

        fn lock_last_known(&self) -> std::sync::MutexGuard<'_, Option<DesktopSnapshot>> {
            self.last_known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn cached_terminal(&self) -> Option<WindowIdentity> {
            let current_hwnd = foreground_hwnd();
            self.snapshot()?
                .terminal
                .filter(|identity| identity.hwnd == current_hwnd)
        }

        fn divider_at(
            &self,
            point: ScreenPoint,
            hit_slop_pixels: i32,
        ) -> Option<(WindowIdentity, PaneDivider)> {
            let current_hwnd = foreground_hwnd();
            let snapshot = self.snapshot()?;
            let target = snapshot
                .terminal
                .filter(|identity| identity.hwnd == current_hwnd)?;
            let divider = snapshot.pane_layout.divider_at(point, hit_slop_pixels)?;
            Some((target, divider))
        }
    }

    /// Splits the hook callback into named keyboard and mouse handlers while
    /// preserving the exact consume/pass decisions of the original closure.
    struct HookDispatcher {
        prefix: PrefixMachine,
        prefix_chord: KeyChord,
        normalizer: KeyboardNormalizer,
        pending_shutdown_key: Option<(PhysicalKey, Instant)>,
        pointer_drag: PointerDragState,
        worker_sender: mpsc::SyncSender<WorkerMessage>,
        shutdown_sender: mpsc::SyncSender<()>,
        dropped_actions_for_hook: Arc<AtomicU64>,
        divider_hit_slop_px: i32,
        snapshots: DesktopSnapshotSource,
        last_cursor_axis: Option<SplitAxis>,
    }

    impl HookDispatcher {
        fn on_keyboard(&mut self, raw: RawKeyEvent) -> HookDecision {
            let terminal = self.snapshots.cached_terminal();
            let event = self.normalizer.normalize(raw, terminal);
            let now = Instant::now();
            let outcome = self.prefix.handle_key_event(event, now);

            if let Some(command) = outcome.command {
                match command {
                    PrefixCommand::Dispatch { target, action } => {
                        let message = if action == TerminalAction::SendPrefixLiteral {
                            WorkerMessage::SendLiteralPrefix {
                                target,
                                chord: self.prefix_chord,
                            }
                        } else {
                            WorkerMessage::Dispatch { target, action }
                        };
                        if self.worker_sender.try_send(message).is_err() {
                            self.dropped_actions_for_hook
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    PrefixCommand::Shutdown => {
                        self.pending_shutdown_key = Some((event.physical_key, now));
                    }
                }
            }

            if shutdown_handshake_should_fire(
                &mut self.pending_shutdown_key,
                event.physical_key,
                event.transition == KeyTransition::Up,
                now,
            ) {
                let _ = self.shutdown_sender.try_send(());
            }

            match outcome.disposition {
                KeyDisposition::PassThrough => HookDecision::Pass,
                KeyDisposition::Consume => HookDecision::Consume,
            }
        }

        fn on_mouse(&mut self, raw: RawMouseEvent) -> HookDecision {
            let point = ScreenPoint::new(raw.x, raw.y);
            match raw.kind {
                MouseEventKind::LeftDown => {
                    let _ = self.prefix.cancel(CancelReason::PointerInput);
                    let divider = self.snapshots.divider_at(point, self.divider_hit_slop_px);
                    let decision = self.pointer_drag.begin(divider, point);
                    self.apply_cursor_axis(decision.cursor_axis());
                    hook_decision(decision.consumes())
                }
                MouseEventKind::Move => {
                    let decision = self.pointer_drag.move_to(foreground_hwnd(), point);
                    if let Some(resize) = decision.resize() {
                        let message = WorkerMessage::ResizePaneByPointer {
                            target: resize.target,
                            focus_point: resize.intent.focus_point,
                            direction: resize.intent.direction,
                            steps: resize.intent.steps,
                            drag_sequence: resize.sequence,
                        };
                        if self.worker_sender.try_send(message).is_err() {
                            self.dropped_actions_for_hook
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    self.apply_cursor_axis(decision.cursor_axis());
                    // Consuming a move freezes the hardware cursor, and a
                    // frozen cursor stops reporting cumulative positions,
                    // so the drag delta would never advance. The initial
                    // button-down was already consumed, so passing moves
                    // through cannot start a text selection in the target.
                    HookDecision::Pass
                }
                MouseEventKind::LeftUp => {
                    let decision = self.pointer_drag.end();
                    self.last_cursor_axis = None;
                    hook_decision(decision.consumes())
                }
            }
        }

        fn apply_cursor_axis(&mut self, axis: Option<SplitAxis>) {
            let Some(axis) = axis else {
                return;
            };
            if self.last_cursor_axis == Some(axis) {
                return;
            }
            self.last_cursor_axis = Some(axis);
            let _ = show_pane_resize_cursor(axis);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::thread;

        use crate::prefix::{LogicalKey, Modifiers};

        use super::*;

        const KEY_A: u8 = 0x1e;
        const KEY_B: u8 = 0x1f;

        fn dispatcher() -> HookDispatcher {
            let (worker_sender, _worker_receiver) = mpsc::sync_channel(4);
            let (shutdown_sender, _shutdown_receiver) = mpsc::sync_channel(1);
            HookDispatcher {
                prefix: PrefixMachine::default(),
                prefix_chord: KeyChord::new(LogicalKey::Character(' '), Modifiers::default()),
                normalizer: KeyboardNormalizer::default(),
                pending_shutdown_key: None,
                pointer_drag: PointerDragState::default(),
                worker_sender,
                shutdown_sender,
                dropped_actions_for_hook: Arc::new(AtomicU64::new(0)),
                divider_hit_slop_px: 8,
                snapshots: DesktopSnapshotSource::new(Arc::new(RwLock::new(
                    DesktopSnapshot::default(),
                ))),
                last_cursor_axis: None,
            }
        }

        #[test]
        fn handshake_fires_on_the_armed_key_up_within_the_window() {
            let armed = Instant::now();
            let mut pending = Some((KEY_A, armed));
            assert!(shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                true,
                armed + SHUTDOWN_HANDSHAKE_WINDOW,
            ));
            assert!(pending.is_none());
        }

        #[test]
        fn handshake_never_fires_without_pending_state() {
            let mut pending: Option<(u8, Instant)> = None;
            assert!(
                !shutdown_handshake_should_fire(&mut pending, KEY_A, true, Instant::now()),
                "a stale release without a handshake must not shut down"
            );
            assert!(pending.is_none());
        }

        #[test]
        fn handshake_ignores_the_pending_key_going_down() {
            let armed = Instant::now();
            let mut pending = Some((KEY_A, armed));
            assert!(!shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                false,
                armed + Duration::from_millis(10),
            ));
            assert!(pending.is_some(), "repeats keep the handshake armed");
            assert!(shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                true,
                armed + Duration::from_millis(20),
            ));
        }

        #[test]
        fn interleaved_other_key_invalidates_the_handshake() {
            let armed = Instant::now();
            let mut pending = Some((KEY_A, armed));
            assert!(!shutdown_handshake_should_fire(
                &mut pending,
                KEY_B,
                false,
                armed + Duration::from_millis(10),
            ));
            assert!(pending.is_none());
            assert!(!shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                true,
                armed + Duration::from_millis(20),
            ));
        }

        #[test]
        fn expired_handshake_drops_state_without_firing() {
            let armed = Instant::now();
            let mut pending = Some((KEY_A, armed));
            assert!(!shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                true,
                armed + SHUTDOWN_HANDSHAKE_WINDOW + Duration::from_millis(1),
            ));
            assert!(pending.is_none());
            assert!(!shutdown_handshake_should_fire(
                &mut pending,
                KEY_A,
                true,
                armed + SHUTDOWN_HANDSHAKE_WINDOW + Duration::from_millis(2),
            ));
        }

        #[test]
        fn cursor_is_reapplied_only_when_the_axis_changes() {
            let mut dispatcher = dispatcher();
            dispatcher.apply_cursor_axis(None);
            assert_eq!(dispatcher.last_cursor_axis, None);

            dispatcher.apply_cursor_axis(Some(SplitAxis::Vertical));
            assert_eq!(dispatcher.last_cursor_axis, Some(SplitAxis::Vertical));
            dispatcher.apply_cursor_axis(Some(SplitAxis::Vertical));
            assert_eq!(dispatcher.last_cursor_axis, Some(SplitAxis::Vertical));

            dispatcher.apply_cursor_axis(Some(SplitAxis::Horizontal));
            assert_eq!(dispatcher.last_cursor_axis, Some(SplitAxis::Horizontal));
        }

        #[test]
        fn left_button_release_resets_cursor_tracking() {
            let mut dispatcher = dispatcher();
            dispatcher.last_cursor_axis = Some(SplitAxis::Vertical);
            let decision = dispatcher.on_mouse(RawMouseEvent {
                x: 0,
                y: 0,
                kind: MouseEventKind::LeftUp,
                injected: false,
                timestamp_ms: 0,
            });
            assert_eq!(decision, HookDecision::Pass);
            assert_eq!(dispatcher.last_cursor_axis, None);
        }

        #[test]
        fn snapshot_source_serves_last_known_snapshot_on_contention() {
            let shared = Arc::new(RwLock::new(DesktopSnapshot::default()));
            let source = DesktopSnapshotSource::new(Arc::clone(&shared));
            let seeded = source.snapshot().expect("first read succeeds");

            let guard = shared.write().expect("lock is free");
            assert_eq!(source.snapshot(), Some(seeded));
            drop(guard);
        }

        #[test]
        fn snapshot_source_survives_a_poisoned_lock() {
            let shared = Arc::new(RwLock::new(DesktopSnapshot::default()));
            let source = DesktopSnapshotSource::new(Arc::clone(&shared));
            let seeded = source.snapshot().expect("first read succeeds");

            let worker_lock = Arc::clone(&shared);
            let _ = thread::spawn(move || {
                let _guard = worker_lock.write().expect("lock starts healthy");
                panic!("poison fixture");
            })
            .join();

            assert_eq!(source.snapshot(), Some(seeded));
        }

        #[test]
        fn map_platform_error_routes_platform_failures_through_the_platform_variant() {
            let error = map_platform_error(PlatformError::HookStartupTerminated);
            assert!(matches!(error, AppError::Platform { .. }));
            assert!(
                std::error::Error::source(&error).is_some(),
                "the PlatformError source chain must survive the mapping"
            );

            let already_running = map_platform_error(PlatformError::AlreadyRunning);
            assert!(matches!(
                already_running,
                AppError::ControllerAlreadyRunning
            ));
        }
    }
}

pub fn run_controller(
    config: &ControllerConfig,
    options: ControllerOptions,
) -> AppResult<ControllerRunReport> {
    #[cfg(target_os = "windows")]
    {
        implementation::run(config, options)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (config, options);
        Err(AppError::UnsupportedPlatform)
    }
}
