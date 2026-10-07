//! Opt-in live bridge probe.
//!
//! Every test in this file is `#[ignore]`d by default because it inspects
//! desktop-wide state or sends a real managed chord to an explicitly
//! identified foreground Windows Terminal window. Run them explicitly with:
//!
//! ```text
//! cargo test --test live_bridge -- --ignored
//! ```
//!
//! Environment variables:
//!
//! - `WINTERMINAL_E2E_ACTION` (required by
//!   `dispatch_bridge_action_from_environment`): the action to dispatch, e.g.
//!   `focus-left`, `split-right`, `new-tab`, `activate-tab-0`. The test panics
//!   when it is missing or names an unsupported action.
//! - `WINTERMINAL_E2E_HWND` (required by `native_pane_geometry_from_environment`,
//!   optional elsewhere): decimal HWND of the dedicated test window. When the
//!   dispatch test leaves it unset, the current foreground window is used and
//!   must be a Windows Terminal window.
//! - `WINTERMINAL_E2E_EXPECTED_TITLE` (optional): when set, the dispatch test
//!   polls until the target window title contains this fragment (5s deadline).
//! - `WINTERMINAL_E2E_EXPECTED_PANES` (optional): expected pane count for the
//!   geometry test; defaults to `2`. The divider assertion only runs when the
//!   expected count is greater than `1`.

use std::env;
use std::thread;
use std::time::{Duration, Instant};

use winterminalp::keymap::binding_for_action;
use winterminalp::pane_layout::PaneLayout;
use winterminalp::platform::windows::{
    HookDecision, InputHook, TerminalAccessibility, foreground_terminal_window, send_bridge_chord,
    terminal_window_identity,
};
use winterminalp::{Direction, TerminalAction};

#[test]
#[ignore = "inspects current desktop global-hotkey reservations"]
fn managed_bridge_chords_are_available_as_global_hotkeys() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey,
        UnregisterHotKey, VK_F1,
    };
    use winterminalp::keymap::managed_bindings;

    for (offset, binding) in managed_bindings().iter().enumerate() {
        let id = 0x5000 + i32::try_from(offset).expect("managed binding count fits in i32");
        let mut modifiers = MOD_NOREPEAT;
        if binding.bridge_chord.ctrl {
            modifiers |= MOD_CONTROL;
        }
        if binding.bridge_chord.alt {
            modifiers |= MOD_ALT;
        }
        if binding.bridge_chord.shift {
            modifiers |= MOD_SHIFT;
        }
        let function_key = binding.bridge_chord.function_key;
        assert!(
            (13..=24).contains(&function_key),
            "{} ({}) uses function key F{function_key}; the production range check in \
             `function_virtual_key` (src/platform/windows/input.rs) only accepts F13-F24",
            binding.action_id,
            binding.bridge_chord
        );
        let virtual_key = u32::from(VK_F1.0) + u32::from(function_key - 1);
        // SAFETY: the generated id is unique for this process and function_key
        // was range-checked to F13-F24 above; no pointers are passed.
        let registered = unsafe {
            RegisterHotKey(None, id, HOT_KEY_MODIFIERS(modifiers.0), virtual_key).is_ok()
        };
        if registered {
            // SAFETY: this unregisters exactly the id successfully registered
            // by this test iteration in the current process.
            let _ = unsafe { UnregisterHotKey(None, id) };
        }
        assert!(
            registered,
            "{} ({}) is reserved by another desktop component",
            binding.action_id, binding.bridge_chord
        );
    }
}

#[test]
#[ignore = "installs process-global keyboard and mouse hooks on the current desktop"]
fn combined_input_hooks_install_and_stop() {
    let hook = InputHook::start(Box::new(|_| HookDecision::Pass), true)
        .expect("combined low-level input hooks should install");
    assert_ne!(hook.thread_id(), 0);
    hook.stop()
        .expect("combined low-level input hooks should stop cleanly");
}

#[test]
#[ignore = "requires a dedicated foreground Terminal window"]
fn dispatch_bridge_action_from_environment() {
    let action_name =
        env::var("WINTERMINAL_E2E_ACTION").expect("WINTERMINAL_E2E_ACTION must be set");
    let action = parse_action(&action_name).expect("unsupported live bridge action");
    let target = match env::var("WINTERMINAL_E2E_HWND") {
        Ok(hwnd) => terminal_window_identity(
            hwnd.parse::<isize>()
                .expect("WINTERMINAL_E2E_HWND must be a decimal HWND"),
        )
        .expect("target identity query should succeed")
        .expect("target must be a Windows Terminal window"),
        Err(_) => foreground_terminal_window()
            .expect("foreground identity query should succeed")
            .expect("foreground window must be Windows Terminal"),
    };
    let binding = binding_for_action(action).expect("action must have a managed bridge binding");

    println!("WINTERMINAL_E2E_TARGET_HWND={}", target.hwnd);
    let receipt = send_bridge_chord(target, binding.bridge_chord)
        .expect("bridge chord should be inserted into the unchanged foreground target");

    assert!(receipt.sent >= 2);
    if let Ok(expected_title) = env::var("WINTERMINAL_E2E_EXPECTED_TITLE") {
        let deadline = Instant::now() + Duration::from_secs(5);
        let title = loop {
            let title = window_title(target.hwnd);
            if title.contains(&expected_title) || Instant::now() >= deadline {
                break title;
            }
            thread::sleep(Duration::from_millis(50));
        };
        println!("WINTERMINAL_E2E_TARGET_TITLE={title}");
        assert!(
            title.contains(&expected_title),
            "target title did not contain {expected_title:?} within 5s of polling; \
             actual title: {title:?}"
        );
    }
}

#[test]
#[ignore = "requires a dedicated Windows Terminal window with split panes"]
fn native_pane_geometry_from_environment() {
    let hwnd = env::var("WINTERMINAL_E2E_HWND")
        .expect("WINTERMINAL_E2E_HWND must be set")
        .parse::<isize>()
        .expect("WINTERMINAL_E2E_HWND must be a decimal HWND");
    let expected_panes = env::var("WINTERMINAL_E2E_EXPECTED_PANES")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("expected pane count is numeric")
        })
        .unwrap_or(2);
    let target = terminal_window_identity(hwnd)
        .expect("target identity query should succeed")
        .expect("target must be a Windows Terminal window");
    let accessibility =
        TerminalAccessibility::initialize().expect("UI Automation should initialize");
    let panes = accessibility
        .pane_geometries(target.hwnd)
        .expect("native TermControl geometry should be readable");
    let layout = PaneLayout::from_panes(panes);

    println!("WINTERMINAL_E2E_PANE_COUNT={}", layout.panes().len());
    println!("WINTERMINAL_E2E_DIVIDER_COUNT={}", layout.dividers().len());
    assert_eq!(layout.panes().len(), expected_panes);
    if expected_panes > 1 {
        assert!(
            !layout.dividers().is_empty(),
            "expected at least one divider for {expected_panes} panes"
        );
    }
    let focused_panes = layout
        .panes()
        .iter()
        .filter(|pane| pane.has_keyboard_focus)
        .count();
    assert!(focused_panes <= 1);
    if foreground_terminal_window().expect("foreground identity query should succeed")
        == Some(target)
    {
        assert_eq!(focused_panes, 1);
    }
}

fn window_title(hwnd: isize) -> String {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: the HWND came from a live identity query and the call has no output pointers.
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0_u16; length.saturating_add(1)];
    // SAFETY: `buffer` is writable for its full length and remains alive for the call.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
}

fn parse_action(value: &str) -> Option<TerminalAction> {
    match value {
        "new-tab" => return Some(TerminalAction::NewTab),
        "next-tab" => return Some(TerminalAction::NextTab),
        "previous-tab" => return Some(TerminalAction::PreviousTab),
        "close-pane" => return Some(TerminalAction::ClosePane),
        "toggle-zoom" => return Some(TerminalAction::TogglePaneZoom),
        _ => {}
    }
    let (name, direction) = value
        .split_once('-')
        .map_or((value, None), |(name, direction)| {
            (name, parse_direction(direction))
        });
    match (name, direction) {
        ("split", Some(direction)) => Some(TerminalAction::SplitPane { direction }),
        ("focus", Some(direction)) => Some(TerminalAction::FocusPane { direction }),
        ("resize", Some(direction)) => Some(TerminalAction::ResizePane { direction }),
        _ => value
            .strip_prefix("activate-tab-")
            .and_then(|index| index.parse::<u8>().ok())
            .filter(|index| *index <= 9)
            .map(|index| TerminalAction::ActivateTab { index }),
    }
}

fn parse_direction(value: &str) -> Option<Direction> {
    match value {
        "left" => Some(Direction::Left),
        "right" => Some(Direction::Right),
        "up" => Some(Direction::Up),
        "down" => Some(Direction::Down),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_probe_actions() {
        assert_eq!(
            parse_action("split-left"),
            Some(TerminalAction::SplitPane {
                direction: Direction::Left,
            })
        );
        assert_eq!(
            parse_action("activate-tab-9"),
            Some(TerminalAction::ActivateTab { index: 9 })
        );
        assert_eq!(parse_action("activate-tab-10"), None);
    }
}
