use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;

use crate::keymap::binding_for_action;
use crate::model::{Direction, TerminalAction, WindowIdentity};
use crate::pane_layout::ScreenPoint;
use crate::platform::windows::{
    PlatformError, TerminalAccessibility, send_bridge_chord, send_literal_chord,
};
use crate::prefix::{KeyChord, LogicalKey};
use crate::{AppError, AppResult};

use super::keyboard::virtual_key_for_logical_key;

const STOP_SEND_ATTEMPTS: usize = 50;
const STOP_SEND_INTERVAL: Duration = Duration::from_millis(20);

/// Failure of a single dispatched action inside the worker thread.
///
/// `Display` reproduces the strings the worker historically stored verbatim,
/// so `WorkerReport.last_dispatch_error` (and the serialized
/// [`ControllerRunReport`](crate::ControllerRunReport)) is unchanged; only
/// the internal typing gains structure. Platform-backed variants keep their
/// [`PlatformError`] source chain.
#[derive(Debug, thiserror::Error)]
pub(super) enum WorkerError {
    /// The registry exposes no bridge chord for the requested action id.
    #[error("no bridge binding exists for {action:?}")]
    MissingBinding { action: TerminalAction },

    /// The configured prefix key has no injectable virtual-key mapping.
    #[error("configured prefix key {key:?} cannot be injected")]
    UnsupportedPrefixKey { key: LogicalKey },

    /// The accessibility subsystem failed to initialize for a pointer resize.
    #[error("{0}")]
    AccessibilityInit(#[source] PlatformError),

    /// Focusing the pane under the pointer failed before the resize steps.
    #[error("{0}")]
    Focus(#[source] PlatformError),

    /// Sending a chord to the target failed (injection state / UIPI).
    #[error("{0}")]
    Injection(#[source] PlatformError),
}

pub(super) enum WorkerMessage {
    Dispatch {
        target: WindowIdentity,
        action: TerminalAction,
    },
    /// Replays the configured Prefix chord so the shell receives it literally.
    SendLiteralPrefix {
        target: WindowIdentity,
        chord: KeyChord,
    },
    ResizePaneByPointer {
        target: WindowIdentity,
        focus_point: ScreenPoint,
        direction: Direction,
        steps: u8,
        /// Monotonic identifier for one pointer drag, so the leading pane is
        /// focused once per drag without depending on a separate end message.
        drag_sequence: u64,
    },
    Stop,
}

#[derive(Default)]
pub(super) struct WorkerReport {
    pub(super) dispatched_actions: u64,
    pub(super) failed_actions: u64,
    pub(super) last_dispatch_error: Option<String>,
}

pub(super) struct ActionWorker {
    sender: Option<SyncSender<WorkerMessage>>,
    join: Option<JoinHandle<AppResult<WorkerReport>>>,
}

impl ActionWorker {
    pub(super) fn start(capacity: usize) -> AppResult<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let join = thread::Builder::new()
            .name("winter-action-worker".to_owned())
            .spawn(move || Ok(run(receiver)))
            .map_err(|error| {
                AppError::Native(format!("failed to spawn action worker thread: {error}"))
            })?;
        Ok(Self {
            sender: Some(sender),
            join: Some(join),
        })
    }

    pub(super) fn sender(&self) -> SyncSender<WorkerMessage> {
        self.sender
            .clone()
            .expect("action worker sender exists until stop()")
    }

    pub(super) fn stop(mut self) -> AppResult<WorkerReport> {
        if let Some(sender) = self.sender.take() {
            request_stop(&sender, STOP_SEND_ATTEMPTS, STOP_SEND_INTERVAL);
        }
        let Some(join) = self.join.take() else {
            return Ok(WorkerReport::default());
        };
        join.join()
            .map_err(|_| AppError::Native("action worker thread panicked".to_owned()))?
    }
}

impl Drop for ActionWorker {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            if let Some(sender) = self.sender.take() {
                request_stop(&sender, STOP_SEND_ATTEMPTS, STOP_SEND_INTERVAL);
            }
            let _ = join.join();
        }
    }
}

/// Enqueues `WorkerMessage::Stop` with a bounded retry budget so a saturated
/// queue cannot hang shutdown; dropping the sender afterwards still lets the
/// worker observe `RecvError` and exit.
fn request_stop(sender: &SyncSender<WorkerMessage>, attempts: usize, interval: Duration) -> bool {
    for _ in 0..attempts {
        match sender.try_send(WorkerMessage::Stop) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(_)) => thread::sleep(interval),
        }
    }
    false
}

fn run(receiver: Receiver<WorkerMessage>) -> WorkerReport {
    let mut report = WorkerReport::default();
    let mut accessibility = None;
    let mut last_focused = None;
    while let Ok(message) = receiver.recv() {
        let result = match message {
            WorkerMessage::Dispatch { target, action } => dispatch_action(target, action),
            WorkerMessage::SendLiteralPrefix { target, chord } => {
                dispatch_literal_prefix(target, chord)
            }
            WorkerMessage::ResizePaneByPointer {
                target,
                focus_point,
                direction,
                steps,
                drag_sequence,
            } => dispatch_pointer_resize(
                &mut accessibility,
                &mut last_focused,
                target,
                focus_point,
                direction,
                steps,
                drag_sequence,
            ),
            WorkerMessage::Stop => break,
        };

        match result {
            Ok(dispatched) => report.dispatched_actions += dispatched,
            Err(error) => {
                report.failed_actions += 1;
                report.last_dispatch_error = Some(error.to_string());
            }
        }
    }
    report
}

fn dispatch_action(target: WindowIdentity, action: TerminalAction) -> Result<u64, WorkerError> {
    let binding =
        binding_for_action(action).ok_or_else(|| WorkerError::MissingBinding { action })?;
    send_bridge_chord(target, binding.bridge_chord)
        .map(|_| 1)
        .map_err(WorkerError::Injection)
}

fn dispatch_literal_prefix(target: WindowIdentity, chord: KeyChord) -> Result<u64, WorkerError> {
    let virtual_key = virtual_key_for_logical_key(chord.key)
        .ok_or_else(|| WorkerError::UnsupportedPrefixKey { key: chord.key })?;
    send_literal_chord(
        target,
        VIRTUAL_KEY(virtual_key),
        chord.modifiers.ctrl,
        chord.modifiers.alt,
        chord.modifiers.shift,
    )
    .map(|_| 1)
    .map_err(WorkerError::Injection)
}

fn dispatch_pointer_resize(
    accessibility: &mut Option<TerminalAccessibility>,
    last_focused: &mut Option<u64>,
    target: WindowIdentity,
    focus_point: ScreenPoint,
    direction: Direction,
    steps: u8,
    drag_sequence: u64,
) -> Result<u64, WorkerError> {
    if steps == 0 {
        return Ok(0);
    }
    if accessibility.is_none() {
        *accessibility =
            Some(TerminalAccessibility::initialize().map_err(WorkerError::AccessibilityInit)?);
    }
    // The leading pane of a divider is stable for the whole drag, so only focus
    // once per drag. Keying on the drag sequence (rather than a separate end
    // message) keeps this correct even when the action queue is saturated.
    if *last_focused != Some(drag_sequence) {
        accessibility
            .as_ref()
            .expect("accessibility is initialized above")
            .focus_pane_at(target, focus_point)
            .map_err(WorkerError::Focus)?;
        *last_focused = Some(drag_sequence);
    }

    let action = TerminalAction::ResizePane { direction };
    let binding =
        binding_for_action(action).ok_or_else(|| WorkerError::MissingBinding { action })?;
    for _ in 0..steps {
        send_bridge_chord(target, binding.bridge_chord).map_err(WorkerError::Injection)?;
    }
    Ok(u64::from(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_enqueue_succeeds_when_the_queue_has_room() {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(request_stop(&sender, 1, Duration::from_millis(1)));
        drop(receiver);
    }

    #[test]
    fn stop_enqueue_gives_up_on_a_saturated_queue() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(WorkerMessage::Stop)
            .expect("queue accepts the first message");
        assert!(!request_stop(&sender, 1, Duration::from_millis(1)));
        drop(receiver);
    }

    #[test]
    fn worker_loop_exits_when_all_senders_drop() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let join = thread::spawn(move || run(receiver));
        drop(sender);
        let report = join.join().expect("worker exits after disconnect");
        assert_eq!(report.dispatched_actions, 0);
        assert_eq!(report.failed_actions, 0);
    }

    #[test]
    fn worker_loop_exits_on_the_stop_message() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let join = thread::spawn(move || run(receiver));
        sender
            .send(WorkerMessage::Stop)
            .expect("stop message enqueues");
        let report = join.join().expect("worker exits after stop");
        assert_eq!(report.dispatched_actions, 0);
    }

    #[test]
    fn stop_returns_the_report_without_blocking() {
        let worker = ActionWorker::start(4).expect("worker starts");
        let sender = worker.sender();
        drop(sender);
        let report = worker.stop().expect("stop succeeds");
        assert_eq!(report.dispatched_actions, 0);
        assert_eq!(report.failed_actions, 0);
    }

    #[test]
    fn worker_error_display_matches_the_reported_dispatch_strings() {
        let missing = WorkerError::MissingBinding {
            action: TerminalAction::NewTab,
        };
        assert_eq!(missing.to_string(), "no bridge binding exists for NewTab");

        let unsupported = WorkerError::UnsupportedPrefixKey {
            key: LogicalKey::Escape,
        };
        assert_eq!(
            unsupported.to_string(),
            "configured prefix key Escape cannot be injected"
        );
    }

    #[test]
    fn worker_error_preserves_the_platform_source_chain() {
        let error = WorkerError::Injection(PlatformError::HookThreadPanicked);

        assert_eq!(
            error.to_string(),
            PlatformError::HookThreadPanicked.to_string(),
            "the report string must stay identical to the raw platform message"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "the PlatformError must remain reachable as the source"
        );
    }
}
