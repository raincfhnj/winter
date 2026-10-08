use std::cell::UnsafeCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HINSTANCE, LPARAM, LRESULT, SetLastError, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, HC_ACTION, HHOOK, HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOWS_HOOK_ID, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::Owned;

use super::error::{PlatformError, PlatformResult};

/// `dwExtraInfo` marker attached to every key synthesized by this controller.
pub const CONTROLLER_INPUT_MARKER: usize = 0x5754_5050;

const SHUTDOWN_POST_ATTEMPTS: u32 = 5;
const SHUTDOWN_POST_RETRY_DELAY: Duration = Duration::from_millis(10);
const SHUTDOWN_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

static ACTIVE_HOOK_STATE: AtomicPtr<HookState> = AtomicPtr::new(ptr::null_mut());

/// Cross-thread health signals polled by the controller while the hook runs.
///
/// All operations are atomic loads/stores, so they never block the hook
/// callback or the controller watchdog thread.
#[derive(Debug)]
struct HookHealth {
    panic_count: AtomicU64,
    enabled: AtomicBool,
}

impl HookHealth {
    fn new() -> Self {
        Self {
            panic_count: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn record_handler_panic(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        self.panic_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTransition {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawKeyEvent {
    pub virtual_key: u32,
    pub scan_code: u32,
    pub transition: KeyTransition,
    pub is_system_key: bool,
    pub is_extended: bool,
    pub is_alt_down: bool,
    pub injected: bool,
    pub timestamp_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Move,
    LeftDown,
    LeftUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMouseEvent {
    pub x: i32,
    pub y: i32,
    pub kind: MouseEventKind,
    pub injected: bool,
    pub timestamp_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInputEvent {
    Keyboard(RawKeyEvent),
    Mouse(RawMouseEvent),
}

impl RawInputEvent {
    fn injected(self) -> bool {
        match self {
            Self::Keyboard(event) => event.injected,
            Self::Mouse(event) => event.injected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDecision {
    Pass,
    Consume,
}

/// Handler called synchronously on the dedicated hook thread.
///
/// It must not perform I/O, wait, pump messages, or run unbounded work. Use a
/// bounded `try_send` to hand an intent to another thread. Injected events are
/// always passed through before this handler is invoked.
pub type RawInputHandler = Box<dyn FnMut(RawInputEvent) -> HookDecision + Send + 'static>;

/// RAII handle for process-global low-level keyboard and optional mouse hooks.
///
/// The callback and Win32 message loop run on a dedicated thread. Dropping the
/// handle posts `WM_QUIT`, joins that thread, and lets `Owned<HHOOK>` call
/// `UnhookWindowsHookEx` on the installer thread.
pub struct InputHook {
    thread_id: u32,
    join: Option<JoinHandle<PlatformResult<()>>>,
    health: Arc<HookHealth>,
}

impl InputHook {
    pub fn start(handler: RawInputHandler, include_mouse: bool) -> PlatformResult<Self> {
        let health = Arc::new(HookHealth::new());
        let thread_health = Arc::clone(&health);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("winter-input-hook".to_owned())
            .spawn(move || hook_thread_main(handler, include_mouse, thread_health, ready_tx))
            .map_err(|source| PlatformError::HookThreadSpawn { source })?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
                health,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => match join.join() {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(PlatformError::HookStartupTerminated),
                Err(_) => Err(PlatformError::HookThreadPanicked),
            },
        }
    }

    #[must_use]
    pub const fn thread_id(&self) -> u32 {
        self.thread_id
    }

    pub fn handler_panic_count(&self) -> u64 {
        self.health.panic_count.load(Ordering::Relaxed)
    }

    pub fn is_handler_active(&self) -> bool {
        self.health.is_enabled()
    }

    pub fn stop(mut self) -> PlatformResult<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> PlatformResult<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };

        let post_result = post_wm_quit(self.thread_id);

        match wait_for_thread_exit(join, SHUTDOWN_JOIN_POLL_INTERVAL, SHUTDOWN_JOIN_TIMEOUT) {
            ThreadExit::Panicked => Err(PlatformError::HookThreadPanicked),
            ThreadExit::Timeout => Err(PlatformError::HookShutdownTimeout),
            ThreadExit::Finished(Err(thread_error)) => Err(thread_error),
            ThreadExit::Finished(Ok(())) => post_result,
        }
    }
}

/// Posts `WM_QUIT` to the hook thread, retrying while its message queue may
/// not exist yet.
fn post_wm_quit(thread_id: u32) -> PlatformResult<()> {
    let mut last_error = None;
    for attempt in 0..SHUTDOWN_POST_ATTEMPTS {
        if attempt != 0 {
            thread::sleep(SHUTDOWN_POST_RETRY_DELAY);
        }
        // SAFETY: `thread_id` belongs to the live hook thread whose message
        // queue is created before start returns; parameters contain no pointers.
        match unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) } {
            Ok(()) => return Ok(()),
            Err(source) => last_error = Some(source),
        }
    }
    let source = last_error.expect("the post retry loop always runs");
    Err(PlatformError::win32("PostThreadMessageW(WM_QUIT)", source))
}

enum ThreadExit {
    Finished(PlatformResult<()>),
    Panicked,
    Timeout,
}

/// Waits up to `timeout` for the hook thread to finish, polling every `poll`.
///
/// On timeout the join handle is dropped, which detaches the thread so the
/// caller is never blocked forever.
fn wait_for_thread_exit(
    join: JoinHandle<PlatformResult<()>>,
    poll: Duration,
    timeout: Duration,
) -> ThreadExit {
    let deadline = Instant::now() + timeout;
    loop {
        if join.is_finished() {
            return match join.join() {
                Ok(result) => ThreadExit::Finished(result),
                Err(_) => ThreadExit::Panicked,
            };
        }
        if Instant::now() >= deadline {
            return ThreadExit::Timeout;
        }
        thread::sleep(poll);
    }
}

impl Drop for InputHook {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct HookState {
    handler: UnsafeCell<RawInputHandler>,
    health: Arc<HookHealth>,
}

impl HookState {
    fn new(handler: RawInputHandler, health: Arc<HookHealth>) -> Self {
        Self {
            handler: UnsafeCell::new(handler),
            health,
        }
    }

    fn handle(&self, event: RawInputEvent) -> HookDecision {
        if !self.health.is_enabled() {
            return HookDecision::Pass;
        }

        // SAFETY: Windows invokes WH_KEYBOARD_LL on the installer thread. The
        // handler is never accessed outside that callback thread, and handlers
        // are forbidden from pumping messages (which would permit reentrancy).
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            (&mut *self.handler.get())(event)
        }));
        match result {
            Ok(decision) => decision,
            Err(_) => {
                self.health.record_handler_panic();
                HookDecision::Pass
            }
        }
    }
}

struct ActiveHookState {
    pointer: *mut HookState,
    _state: Box<HookState>,
}

impl ActiveHookState {
    fn install(handler: RawInputHandler, health: Arc<HookHealth>) -> PlatformResult<Self> {
        let mut state = Box::new(HookState::new(handler, health));
        let pointer = state.as_mut() as *mut HookState;
        ACTIVE_HOOK_STATE
            .compare_exchange(
                ptr::null_mut(),
                pointer,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| PlatformError::HookAlreadyActive)?;

        Ok(Self {
            pointer,
            _state: state,
        })
    }
}

impl Drop for ActiveHookState {
    fn drop(&mut self) {
        let _ = ACTIVE_HOOK_STATE.compare_exchange(
            self.pointer,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

fn hook_thread_main(
    handler: RawInputHandler,
    include_mouse: bool,
    health: Arc<HookHealth>,
    ready: mpsc::SyncSender<PlatformResult<u32>>,
) -> PlatformResult<()> {
    if let Err(error) = ensure_message_queue() {
        let _ = ready.send(Err(error));
        return Ok(());
    }

    let active_state = match ActiveHookState::install(handler, health) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let keyboard_hook = match install_hook(
        WH_KEYBOARD_LL,
        Some(low_level_keyboard_proc),
        "SetWindowsHookExW(WH_KEYBOARD_LL)",
    ) {
        Ok(hook) => hook,
        Err(error) => {
            drop(active_state);
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let mouse_hook = if include_mouse {
        match install_hook(
            WH_MOUSE_LL,
            Some(low_level_mouse_proc),
            "SetWindowsHookExW(WH_MOUSE_LL)",
        ) {
            Ok(hook) => Some(hook),
            Err(error) => {
                drop(keyboard_hook);
                drop(active_state);
                let _ = ready.send(Err(error));
                return Ok(());
            }
        }
    } else {
        None
    };
    // SAFETY: GetCurrentThreadId takes no arguments and has no ownership rules.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready.send(Ok(thread_id)).is_err() {
        return Ok(());
    }

    let result = run_message_loop();
    drop(mouse_hook);
    drop(keyboard_hook);
    drop(active_state);
    result
}

/// Creates this thread's Win32 message queue so `PostThreadMessageW(WM_QUIT)`
/// can reach the hook thread after `start` returns.
fn ensure_message_queue() -> PlatformResult<()> {
    let mut initial_message = MSG::default();
    // SAFETY: SetLastError only writes this thread's last-error slot, which is
    // cleared first so a stale code cannot be blamed on PeekMessageW.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: `initial_message` is a valid writable MSG and PM_NOREMOVE leaves
    // any message in place while still creating the thread message queue. A
    // FALSE result with a zero last-error means "queue empty", not failure.
    let peeked = unsafe { PeekMessageW(&mut initial_message, None, 0, 0, PM_NOREMOVE) };
    if peeked.as_bool() {
        return Ok(());
    }
    // SAFETY: GetLastError reads this thread's last-error slot immediately
    // after PeekMessageW, before any other Win32 call can overwrite it.
    let os_error = unsafe { GetLastError() };
    if os_error == ERROR_SUCCESS {
        return Ok(());
    }
    Err(PlatformError::win32(
        "PeekMessageW",
        windows::core::Error::from_thread(),
    ))
}

fn install_hook(
    hook_id: WINDOWS_HOOK_ID,
    proc: HOOKPROC,
    operation: &'static str,
) -> PlatformResult<Owned<HHOOK>> {
    // SAFETY: None requests the module handle for the current process; the
    // returned borrowed handle remains loaded for the process lifetime.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|source| PlatformError::win32("GetModuleHandleW", source))?;
    let instance = HINSTANCE(module.0);
    // SAFETY: the callback uses the required system ABI, the module remains
    // loaded, and thread id 0 requests the documented global low-level hook.
    let hook = unsafe { SetWindowsHookExW(hook_id, proc, Some(instance), 0) }
        .map_err(|source| PlatformError::win32(operation, source))?;

    // SAFETY: SetWindowsHookExW returned a valid hook handle and this function
    // transfers its sole ownership to Owned for RAII unhooking.
    Ok(unsafe { Owned::new(hook) })
}

fn run_message_loop() -> PlatformResult<()> {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a valid writable MSG owned by this hook thread.
        let result = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut message, None, 0, 0)
        };
        match result.0 {
            -1 => {
                return Err(PlatformError::win32(
                    "GetMessageW",
                    windows::core::Error::from_thread(),
                ));
            }
            0 => return Ok(()),
            // SAFETY: `message` was populated successfully by GetMessageW and
            // remains valid for translation and dispatch.
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != HC_ACTION as i32 || lparam.0 == 0 {
        // SAFETY: forwarding the original callback parameters is required by
        // the hook contract when this callback does not handle the event.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: for HC_ACTION Windows documents lparam as a valid pointer to a
    // KBDLLHOOKSTRUCT for the duration of this callback.
    let data = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let Some(event) = raw_key_event_from_hook(wparam.0 as u32, data) else {
        // SAFETY: forwarding the unchanged parameters preserves the hook chain.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };

    route_hook_event(code, wparam, lparam, RawInputEvent::Keyboard(event))
}

unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != HC_ACTION as i32 || lparam.0 == 0 {
        // SAFETY: forwarding the original callback parameters is required by
        // the hook contract when this callback does not handle the event.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: for HC_ACTION Windows documents lparam as a valid pointer to an
    // MSLLHOOKSTRUCT for the duration of this callback.
    let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let Some(event) = raw_mouse_event_from_hook(wparam.0 as u32, data) else {
        // SAFETY: forwarding the unchanged parameters preserves the hook chain.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };

    route_hook_event(code, wparam, lparam, RawInputEvent::Mouse(event))
}

/// Applies the injected-event filter and the shared handler/dispatch decision
/// to an event already decoded by a low-level hook callback.
fn route_hook_event(code: i32, wparam: WPARAM, lparam: LPARAM, event: RawInputEvent) -> LRESULT {
    // Never feed synthetic input back into the prefix state machine. In
    // particular, this prevents the synthetic high-function-key bridge from
    // recursively firing.
    if event.injected() {
        // SAFETY: forwarding the unchanged parameters preserves the hook chain.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let state = ACTIVE_HOOK_STATE.load(Ordering::Acquire);
    if state.is_null() {
        // SAFETY: forwarding the unchanged parameters preserves the hook chain.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: ActiveHookState publishes this pointer before hook installation,
    // clears it only after both hooks are dropped, and callbacks are serialized
    // on the installer thread.
    let decision = unsafe { (&*state).handle(event) };
    match decision {
        // SAFETY: forwarding the unchanged parameters preserves the hook chain.
        HookDecision::Pass => unsafe { CallNextHookEx(None, code, wparam, lparam) },
        HookDecision::Consume => LRESULT(1),
    }
}

fn raw_key_event_from_hook(message: u32, data: &KBDLLHOOKSTRUCT) -> Option<RawKeyEvent> {
    let (transition, is_system_key) = match message {
        WM_KEYDOWN => (KeyTransition::Down, false),
        WM_KEYUP => (KeyTransition::Up, false),
        WM_SYSKEYDOWN => (KeyTransition::Down, true),
        WM_SYSKEYUP => (KeyTransition::Up, true),
        _ => return None,
    };

    Some(RawKeyEvent {
        virtual_key: data.vkCode,
        scan_code: data.scanCode,
        transition,
        is_system_key,
        is_extended: data.flags.contains(LLKHF_EXTENDED),
        is_alt_down: data.flags.contains(LLKHF_ALTDOWN),
        injected: data.flags.contains(LLKHF_INJECTED)
            || data.dwExtraInfo == CONTROLLER_INPUT_MARKER,
        timestamp_ms: data.time,
    })
}

fn raw_mouse_event_from_hook(message: u32, data: &MSLLHOOKSTRUCT) -> Option<RawMouseEvent> {
    let kind = match message {
        WM_MOUSEMOVE => MouseEventKind::Move,
        WM_LBUTTONDOWN => MouseEventKind::LeftDown,
        WM_LBUTTONUP => MouseEventKind::LeftUp,
        _ => return None,
    };

    Some(RawMouseEvent {
        x: data.pt.x,
        y: data.pt.y,
        kind,
        injected: data.flags & LLMHF_INJECTED != 0 || data.dwExtraInfo == CONTROLLER_INPUT_MARKER,
        timestamp_ms: data.time,
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{KBDLLHOOKSTRUCT_FLAGS, LLKHF_INJECTED};

    use super::*;

    #[test]
    fn decodes_physical_key_transition() {
        let event = raw_key_event_from_hook(
            WM_KEYDOWN,
            &KBDLLHOOKSTRUCT {
                vkCode: 0x42,
                scanCode: 0x30,
                flags: KBDLLHOOKSTRUCT_FLAGS::default(),
                time: 123,
                dwExtraInfo: 0,
            },
        )
        .expect("known keyboard message");

        assert_eq!(event.transition, KeyTransition::Down);
        assert_eq!(event.virtual_key, 0x42);
        assert!(!event.injected);
        assert!(!event.is_system_key);
    }

    #[test]
    fn marks_injected_system_key_release() {
        let event = raw_key_event_from_hook(
            WM_SYSKEYUP,
            &KBDLLHOOKSTRUCT {
                vkCode: 0x7c,
                scanCode: 0,
                flags: LLKHF_INJECTED,
                time: 456,
                dwExtraInfo: CONTROLLER_INPUT_MARKER,
            },
        )
        .expect("known keyboard message");

        assert_eq!(event.transition, KeyTransition::Up);
        assert!(event.injected);
        assert!(event.is_system_key);
    }

    #[test]
    fn ignores_non_keyboard_messages() {
        assert!(
            raw_key_event_from_hook(0xffff, &KBDLLHOOKSTRUCT::default()).is_none(),
            "unknown messages must pass through"
        );
    }

    #[test]
    fn decodes_mouse_drag_events_and_injection_flags() {
        let event = raw_mouse_event_from_hook(
            WM_LBUTTONDOWN,
            &MSLLHOOKSTRUCT {
                pt: windows::Win32::Foundation::POINT { x: 320, y: 240 },
                mouseData: 0,
                flags: LLMHF_INJECTED,
                time: 789,
                dwExtraInfo: 123,
            },
        )
        .expect("known mouse message");

        assert_eq!(event.kind, MouseEventKind::LeftDown);
        assert_eq!((event.x, event.y), (320, 240));
        assert!(event.injected);
    }

    #[test]
    fn ignores_unneeded_mouse_messages() {
        assert!(
            raw_mouse_event_from_hook(0xffff, &MSLLHOOKSTRUCT::default()).is_none(),
            "unknown messages must pass through"
        );
    }

    #[test]
    fn marks_controller_marker_injected_without_os_flag() {
        let key = raw_key_event_from_hook(
            WM_KEYDOWN,
            &KBDLLHOOKSTRUCT {
                vkCode: 0x41,
                scanCode: 0x1e,
                flags: KBDLLHOOKSTRUCT_FLAGS::default(),
                time: 1,
                dwExtraInfo: CONTROLLER_INPUT_MARKER,
            },
        )
        .expect("known keyboard message");
        assert!(key.injected);

        let mouse = raw_mouse_event_from_hook(
            WM_MOUSEMOVE,
            &MSLLHOOKSTRUCT {
                pt: windows::Win32::Foundation::POINT { x: 1, y: 2 },
                mouseData: 0,
                flags: Default::default(),
                time: 1,
                dwExtraInfo: CONTROLLER_INPUT_MARKER,
            },
        )
        .expect("known mouse message");
        assert!(mouse.injected);
    }

    #[test]
    fn keeps_physical_input_unmarked_as_not_injected() {
        let key = raw_key_event_from_hook(
            WM_KEYDOWN,
            &KBDLLHOOKSTRUCT {
                vkCode: 0x41,
                scanCode: 0x1e,
                flags: KBDLLHOOKSTRUCT_FLAGS::default(),
                time: 1,
                dwExtraInfo: 0,
            },
        )
        .expect("known keyboard message");
        assert!(!key.injected);
    }

    fn keyboard_event() -> RawInputEvent {
        RawInputEvent::Keyboard(RawKeyEvent {
            virtual_key: 0x41,
            scan_code: 0x1e,
            transition: KeyTransition::Down,
            is_system_key: false,
            is_extended: false,
            is_alt_down: false,
            injected: false,
            timestamp_ms: 0,
        })
    }

    #[test]
    fn handler_panic_disables_handler_and_counts_once() {
        let health = Arc::new(HookHealth::new());
        let state = HookState::new(
            Box::new(|_event: RawInputEvent| -> HookDecision { panic!("handler failure") }),
            Arc::clone(&health),
        );

        assert!(health.is_enabled());
        assert_eq!(state.handle(keyboard_event()), HookDecision::Pass);
        assert!(!health.is_enabled());
        assert_eq!(health.panic_count.load(Ordering::Relaxed), 1);

        assert_eq!(state.handle(keyboard_event()), HookDecision::Pass);
        assert_eq!(health.panic_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn healthy_handler_stays_active_without_panics() {
        let health = Arc::new(HookHealth::new());
        let state = HookState::new(Box::new(|_| HookDecision::Consume), Arc::clone(&health));

        assert_eq!(state.handle(keyboard_event()), HookDecision::Consume);
        assert!(health.is_enabled());
        assert_eq!(health.panic_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn post_wm_quit_fails_for_thread_without_queue() {
        let error = post_wm_quit(0).expect_err("thread 0 never owns a message queue");
        assert!(matches!(
            error,
            PlatformError::Win32 {
                operation: "PostThreadMessageW(WM_QUIT)",
                ..
            }
        ));
    }

    #[test]
    fn post_wm_quit_reaches_thread_with_message_queue() {
        let (id_tx, id_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut message = MSG::default();
            // SAFETY: `message` is a valid writable MSG and PM_NOREMOVE creates
            // this worker's message queue without removing any message.
            unsafe {
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            }
            // SAFETY: GetCurrentThreadId takes no arguments and has no ownership rules.
            let worker_id = unsafe { GetCurrentThreadId() };
            id_tx
                .send(worker_id)
                .expect("test receiver stays alive until the id is sent");
            loop {
                // SAFETY: `message` is a valid writable MSG owned by this worker.
                let result = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut message, None, 0, 0)
                };
                match result.0 {
                    -1 => panic!("GetMessageW failed"),
                    0 => return,
                    _ => {}
                }
            }
        });

        let worker_id = id_rx.recv().expect("worker reports its thread id");
        post_wm_quit(worker_id).expect("queue exists so WM_QUIT must post");
        worker.join().expect("worker exits when WM_QUIT arrives");
    }

    #[test]
    fn wait_for_thread_exit_reports_results() {
        let ok = thread::spawn(|| PlatformResult::<()>::Ok(()));
        assert!(matches!(
            wait_for_thread_exit(ok, Duration::from_millis(5), Duration::from_secs(2)),
            ThreadExit::Finished(Ok(()))
        ));

        let err = thread::spawn(|| PlatformResult::<()>::Err(PlatformError::HookStartupTerminated));
        assert!(matches!(
            wait_for_thread_exit(err, Duration::from_millis(5), Duration::from_secs(2)),
            ThreadExit::Finished(Err(PlatformError::HookStartupTerminated))
        ));

        let panicked = thread::spawn(|| -> PlatformResult<()> { panic!("hook thread crashed") });
        assert!(matches!(
            wait_for_thread_exit(panicked, Duration::from_millis(5), Duration::from_secs(2)),
            ThreadExit::Panicked
        ));
    }

    #[test]
    fn wait_for_thread_exit_times_out_and_detaches() {
        let join = thread::spawn(|| {
            thread::sleep(Duration::from_millis(300));
            PlatformResult::<()>::Ok(())
        });
        let outcome =
            wait_for_thread_exit(join, Duration::from_millis(5), Duration::from_millis(50));
        assert!(matches!(outcome, ThreadExit::Timeout));
    }

    #[test]
    fn shutdown_prefers_thread_error_over_post_failure() {
        let join =
            thread::spawn(|| PlatformResult::<()>::Err(PlatformError::HookStartupTerminated));
        let mut hook = InputHook {
            thread_id: 0,
            join: Some(join),
            health: Arc::new(HookHealth::new()),
        };
        let error = hook
            .shutdown()
            .expect_err("thread error must win over post error");
        assert!(matches!(error, PlatformError::HookStartupTerminated));
    }

    #[test]
    fn shutdown_reports_post_failure_when_thread_exits_cleanly() {
        let join = thread::spawn(|| PlatformResult::<()>::Ok(()));
        let mut hook = InputHook {
            thread_id: 0,
            join: Some(join),
            health: Arc::new(HookHealth::new()),
        };
        let error = hook
            .shutdown()
            .expect_err("post failure must surface when the thread is clean");
        assert!(matches!(
            error,
            PlatformError::Win32 {
                operation: "PostThreadMessageW(WM_QUIT)",
                ..
            }
        ));
    }

    #[test]
    fn shutdown_reports_hook_thread_panic() {
        let join = thread::spawn(|| -> PlatformResult<()> { panic!("hook thread crashed") });
        let mut hook = InputHook {
            thread_id: 0,
            join: Some(join),
            health: Arc::new(HookHealth::new()),
        };
        let error = hook.shutdown().expect_err("panic must be reported");
        assert!(matches!(error, PlatformError::HookThreadPanicked));
    }

    #[test]
    fn ensure_message_queue_succeeds_for_caller_thread() {
        ensure_message_queue().expect("an empty queue must not count as a failure");
    }

    #[test]
    fn input_hook_exposes_health_signals() {
        let health = Arc::new(HookHealth::new());
        let join = thread::spawn(|| PlatformResult::<()>::Ok(()));
        let hook = InputHook {
            thread_id: 0,
            join: Some(join),
            health: Arc::clone(&health),
        };

        assert!(hook.is_handler_active());
        assert_eq!(hook.handler_panic_count(), 0);

        health.record_handler_panic();
        assert!(!hook.is_handler_active());
        assert_eq!(hook.handler_panic_count(), 1);

        drop(hook);
    }
}
