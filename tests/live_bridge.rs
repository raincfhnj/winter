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
//! - `WINTERMINAL_E2E_DRAG_PHASE` (`baseline` or `verify`), plus
//!   `WINTERMINAL_E2E_DRAG_POINT=x,y` for the baseline phase: drives
//!   `pointer_drag_moves_only_the_dragged_divider`, the two-phase probe for the
//!   pointer-drag path. Optional: `WINTERMINAL_E2E_DRAG_BASELINE` (snapshot
//!   path, default `%TEMP%\winter-drag-baseline.txt`),
//!   `WINTERMINAL_E2E_DRAG_DELTA` (pixels the operator drags, default `120`),
//!   `WINTERMINAL_E2E_DRAG_DIRECTION` (`left|right|up|down`),
//!   `WINTERMINAL_E2E_DRAG_EXPECT` (`move` or `pass-through`),
//!   `WINTERMINAL_E2E_DRAG_ALLOW_NATIVE=1`.

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use winter::keymap::binding_for_action;
use winter::pane_layout::{PaneLayout, ScreenPoint, SplitAxis};
use winter::platform::windows::{
    HookDecision, InputHook, TerminalAccessibility, foreground_terminal_window, send_bridge_chord,
    terminal_window_identity,
};
use winter::{Direction, TerminalAction};

#[test]
#[ignore = "inspects current desktop global-hotkey reservations"]
fn managed_bridge_chords_are_available_as_global_hotkeys() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey,
        UnregisterHotKey, VK_F1,
    };
    use winter::keymap::managed_bindings;

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
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
    };

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

    // `plan_chord_events` (src/platform/windows/input.rs) synthesizes a
    // key-down plus key-up for every required chord modifier that is not
    // already physically held, then the target key down/up, then releases
    // only the modifiers it synthesized. Every managed bridge chord uses two
    // or three of ctrl/alt/shift, so with no modifiers held the plan is 6 or
    // 8 events; each already-held required modifier removes its down/up pair.
    // Reading the same physical state `send_bridge_chord` snapshots keeps the
    // expected count exact even when the operator holds a required modifier.
    let chord = binding.bridge_chord;
    let mut synthesized_modifiers = 0_u32;
    for (required, virtual_key) in [
        (chord.ctrl, VK_CONTROL),
        (chord.alt, VK_MENU),
        (chord.shift, VK_SHIFT),
    ] {
        if !required {
            continue;
        }
        // SAFETY: GetAsyncKeyState takes a virtual-key code by value and has
        // no pointer or ownership requirements.
        if unsafe { GetAsyncKeyState(i32::from(virtual_key.0)) } >= 0 {
            synthesized_modifiers += 1;
        }
    }
    let expected_events = synthesized_modifiers * 2 + 2;
    assert_eq!(
        receipt.sent, expected_events,
        "dispatched event count must match the planner's plan for {chord:?}"
    );
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
    for (index, pane) in layout.panes().iter().enumerate() {
        println!("WINTERMINAL_E2E_PANE_{index}_TITLE={:?}", pane.title);
    }
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

/// Two-phase probe for the pointer-drag path on a dedicated Terminal window.
///
/// The bridge never injects pointer input, so one run cannot drag anything: the
/// operator performs the drag between the two phases.
///
/// Reproduce the P1-2 check on a three-pane row (`split-right`, then
/// `split-right` again so the layout is `[A|B|C]`):
///
/// 1. Position the pointer over the `B|C` divider and run the baseline phase:
///
///    ```text
///    $env:WINTERMINAL_E2E_HWND="1234"
///    $env:WINTERMINAL_E2E_DRAG_PHASE="baseline"
///    $env:WINTERMINAL_E2E_DRAG_POINT="600,250"   # on the B|C divider
///    cargo test --locked --test live_bridge -- --ignored --exact \
///      pointer_drag_moves_only_the_dragged_divider --nocapture
///    ```
///
///    Expect `WINTERMINAL_E2E_DRAG_EXPECT=move` and a focus target inside `C`
///    (the pane on the trailing side), *not* inside `B`: focusing `B` is what
///    made Windows Terminal resize `A|B` instead.
/// 2. Drag that divider `WINTERMINAL_E2E_DRAG_DELTA` (default 120) pixels to
///    the right, slowly enough to produce more than one resize step, then
///    release the button.
/// 3. Run the same command with `WINTERMINAL_E2E_DRAG_PHASE="verify"` and
///    `WINTERMINAL_E2E_DRAG_DIRECTION="right"`. The probe re-reads the window
///    and asserts that the dragged line moved by at least half the requested
///    distance in the requested direction *and that every other divider line
///    kept its coordinate*. A pre-fix run moves the `A|B` line instead, which
///    fails that last assertion.
///
/// For a divider whose owning splitter cannot be proven from the rectangles
/// (e.g. the middle divider of four panes in a row) the baseline phase reports
/// `WINTERMINAL_E2E_DRAG_EXPECT=pass-through`: the bridge must not capture the
/// drag at all. Run steps 1-3 with that expectation and the probe asserts the
/// dragged line did not move either — unless `WINTERMINAL_E2E_DRAG_ALLOW_NATIVE=1`
/// is set, which records the result without asserting it when the installed
/// Windows Terminal implements divider dragging itself.
#[test]
#[ignore = "requires a dedicated Windows Terminal window and a manual pointer drag"]
fn pointer_drag_moves_only_the_dragged_divider() {
    let phase = env::var("WINTERMINAL_E2E_DRAG_PHASE")
        .expect("WINTERMINAL_E2E_DRAG_PHASE must be set to baseline or verify");
    let layout = live_pane_layout();
    let vertical = divider_coordinates(&layout, SplitAxis::Vertical);
    let horizontal = divider_coordinates(&layout, SplitAxis::Horizontal);
    let delta = drag_delta();

    match phase.as_str() {
        "baseline" => {
            let point = parse_point(
                &env::var("WINTERMINAL_E2E_DRAG_POINT")
                    .expect("WINTERMINAL_E2E_DRAG_POINT must be set to x,y on the divider to drag"),
            );
            let divider = layout.divider_at(point, 0).unwrap_or_else(|| {
                panic!("no divider at {point:?}; vertical {vertical:?} / horizontal {horizontal:?}")
            });
            let focus = divider.resize_focus_point();
            let expect = if focus.is_some() {
                "move"
            } else {
                "pass-through"
            };
            println!(
                "WINTERMINAL_E2E_DRAG_DIVIDER={:?}@{} span {}..{}",
                divider.axis(),
                divider.coordinate(),
                divider.span_start(),
                divider.span_end()
            );
            println!("WINTERMINAL_E2E_DRAG_EXPECT={expect}");
            println!("WINTERMINAL_E2E_DRAG_FOCUS={focus:?}");
            if let Some(focus) = focus {
                let title = layout
                    .panes()
                    .iter()
                    .find(|pane| pane.bounds.contains(focus))
                    .map_or("<no pane>", |pane| pane.title.as_str());
                println!("WINTERMINAL_E2E_DRAG_FOCUS_PANE={title:?}");
            }
            println!(
                "WINTERMINAL_E2E_DRAG_VERTICAL={}",
                format_coordinates(&vertical)
            );
            println!(
                "WINTERMINAL_E2E_DRAG_HORIZONTAL={}",
                format_coordinates(&horizontal)
            );

            let baseline = format!(
                "point={},{}\naxis={}\ncoordinate={}\nexpect={expect}\nvertical={}\nhorizontal={}\n",
                point.x,
                point.y,
                axis_name(divider.axis()),
                divider.coordinate(),
                format_coordinates(&vertical),
                format_coordinates(&horizontal),
            );
            let path = baseline_path();
            std::fs::write(&path, baseline).expect("the baseline snapshot must be writable");
            println!("WINTERMINAL_E2E_DRAG_BASELINE={}", path.display());
            println!(
                "Drag the {expect} divider at ({}, {}) by about {delta}px, release, then run the \
                 verify phase.",
                point.x, point.y
            );
        }
        "verify" => {
            let path = baseline_path();
            let baseline = DragBaseline::parse(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
            );
            println!("WINTERMINAL_E2E_DRAG_BASELINE={}", path.display());
            println!(
                "WINTERMINAL_E2E_DRAG_BASELINE_LINES={:?}",
                (dragged_list(&baseline), other_list(&baseline))
            );
            println!(
                "WINTERMINAL_E2E_DRAG_CURRENT_LINES={:?}",
                (
                    dragged_current(&baseline, &vertical, &horizontal),
                    other_current(&baseline, &vertical, &horizontal)
                )
            );

            // Core P1-2 contract: whatever the bridge decided, it must never
            // move a divider other than the one under the pointer.
            let (other_gone, other_appeared) = unmatched_coordinates(
                other_list(&baseline),
                &other_current(&baseline, &vertical, &horizontal),
                1,
            );
            assert!(
                other_gone.is_empty() && other_appeared.is_empty(),
                "a divider other than the dragged one moved: gone {other_gone:?}, \
                 appeared {other_appeared:?}"
            );

            let (gone, appeared) = unmatched_coordinates(
                dragged_list(&baseline),
                &dragged_current(&baseline, &vertical, &horizontal),
                1,
            );
            match baseline.expect.as_str() {
                "move" => {
                    assert!(
                        gone.is_empty(),
                        "the dragged divider at {} disappeared: gone {gone:?}",
                        baseline.coordinate
                    );
                    assert_eq!(
                        appeared.len(),
                        1,
                        "exactly one dragged divider line must appear, got {appeared:?}"
                    );
                    let moved = appeared[0] - baseline.coordinate;
                    assert!(
                        moved.abs() >= (delta / 2).max(1),
                        "the dragged divider moved {moved}px, expected at least {}px for a \
                         {delta}px drag",
                        (delta / 2).max(1)
                    );
                    if let Ok(name) = env::var("WINTERMINAL_E2E_DRAG_DIRECTION") {
                        let direction = parse_direction(&name)
                            .expect("WINTERMINAL_E2E_DRAG_DIRECTION must be left/right/up/down");
                        let positive = matches!(direction, Direction::Right | Direction::Down);
                        assert_eq!(
                            moved > 0,
                            positive,
                            "the dragged divider moved {moved}px, opposite to {name:?}"
                        );
                    }
                }
                "pass-through" => {
                    if env::var("WINTERMINAL_E2E_DRAG_ALLOW_NATIVE").as_deref() == Ok("1") {
                        println!("WINTERMINAL_E2E_DRAG_UNCAPTURED_MOVE={gone:?}->{appeared:?}");
                    } else {
                        assert!(
                            gone.is_empty() && appeared.is_empty(),
                            "the divider was reported as pass-through but moved: \
                             {gone:?}->{appeared:?}"
                        );
                    }
                }
                other => panic!("unknown WINTERMINAL_E2E_DRAG_EXPECT {other:?} in the baseline"),
            }
        }
        other => panic!("WINTERMINAL_E2E_DRAG_PHASE must be baseline or verify, got {other:?}"),
    }
}

/// The pane layout of the window named by `WINTERMINAL_E2E_HWND`.
fn live_pane_layout() -> PaneLayout {
    let hwnd = env::var("WINTERMINAL_E2E_HWND")
        .expect("WINTERMINAL_E2E_HWND must be set")
        .parse::<isize>()
        .expect("WINTERMINAL_E2E_HWND must be a decimal HWND");
    let target = terminal_window_identity(hwnd)
        .expect("target identity query should succeed")
        .expect("target must be a Windows Terminal window");
    let accessibility =
        TerminalAccessibility::initialize().expect("UI Automation should initialize");
    let panes = accessibility
        .pane_geometries(target.hwnd)
        .expect("native TermControl geometry should be readable");
    PaneLayout::from_panes(panes)
}

/// Distinct divider coordinates on `axis`, sorted.
fn divider_coordinates(layout: &PaneLayout, axis: SplitAxis) -> Vec<i32> {
    let mut coordinates = layout
        .dividers()
        .iter()
        .filter(|divider| divider.axis() == axis)
        .map(|divider| divider.coordinate())
        .collect::<Vec<_>>();
    coordinates.sort_unstable();
    coordinates.dedup();
    coordinates
}

fn drag_delta() -> i32 {
    env::var("WINTERMINAL_E2E_DRAG_DELTA")
        .ok()
        .map_or(120, |value| {
            value
                .parse()
                .expect("WINTERMINAL_E2E_DRAG_DELTA must be numeric")
        })
}

fn baseline_path() -> PathBuf {
    env::var_os("WINTERMINAL_E2E_DRAG_BASELINE").map_or_else(
        || env::temp_dir().join("winter-drag-baseline.txt"),
        PathBuf::from,
    )
}

const fn axis_name(axis: SplitAxis) -> &'static str {
    match axis {
        SplitAxis::Vertical => "vertical",
        SplitAxis::Horizontal => "horizontal",
    }
}

fn format_coordinates(coordinates: &[i32]) -> String {
    coordinates
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_coordinates(value: &str) -> Vec<i32> {
    value
        .split(',')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            part.trim()
                .parse()
                .expect("a divider coordinate must be numeric")
        })
        .collect()
}

fn parse_point(value: &str) -> ScreenPoint {
    let (x, y) = value
        .split_once(',')
        .expect("a point must be written as x,y");
    ScreenPoint::new(
        x.trim().parse().expect("x must be numeric"),
        y.trim().parse().expect("y must be numeric"),
    )
}

/// Pairs every baseline coordinate with a current one within `tolerance`,
/// returning the unmatched baseline and current coordinates.
fn unmatched_coordinates(
    baseline: &[i32],
    current: &[i32],
    tolerance: i32,
) -> (Vec<i32>, Vec<i32>) {
    let mut remaining = current.to_vec();
    let mut missing = Vec::new();
    for coordinate in baseline {
        match remaining
            .iter()
            .position(|candidate| (candidate - coordinate).abs() <= tolerance)
        {
            Some(index) => {
                remaining.remove(index);
            }
            None => missing.push(*coordinate),
        }
    }
    (missing, remaining)
}

/// The divider lines recorded by the baseline phase, and which of them the
/// operator dragged.
struct DragBaseline {
    axis: SplitAxis,
    coordinate: i32,
    expect: String,
    vertical: Vec<i32>,
    horizontal: Vec<i32>,
}

impl DragBaseline {
    fn parse(raw: &str) -> Self {
        let mut axis = None;
        let mut coordinate = None;
        let mut expect = None;
        let mut vertical = Vec::new();
        let mut horizontal = Vec::new();
        for line in raw.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "axis" => {
                    axis = Some(match value.trim() {
                        "vertical" => SplitAxis::Vertical,
                        "horizontal" => SplitAxis::Horizontal,
                        other => panic!("unknown baseline axis {other:?}"),
                    });
                }
                "coordinate" => {
                    coordinate = Some(
                        value
                            .trim()
                            .parse()
                            .expect("the baseline coordinate must be numeric"),
                    );
                }
                "expect" => expect = Some(value.trim().to_owned()),
                "vertical" => vertical = parse_coordinates(value),
                "horizontal" => horizontal = parse_coordinates(value),
                _ => {}
            }
        }
        Self {
            axis: axis.expect("the baseline must record the dragged divider axis"),
            coordinate: coordinate.expect("the baseline must record the dragged coordinate"),
            expect: expect.expect("the baseline must record the expected capture decision"),
            vertical,
            horizontal,
        }
    }
}

fn dragged_list(baseline: &DragBaseline) -> &[i32] {
    match baseline.axis {
        SplitAxis::Vertical => &baseline.vertical,
        SplitAxis::Horizontal => &baseline.horizontal,
    }
}

fn other_list(baseline: &DragBaseline) -> &[i32] {
    match baseline.axis {
        SplitAxis::Vertical => &baseline.horizontal,
        SplitAxis::Horizontal => &baseline.vertical,
    }
}

fn dragged_current(baseline: &DragBaseline, vertical: &[i32], horizontal: &[i32]) -> Vec<i32> {
    match baseline.axis {
        SplitAxis::Vertical => vertical.to_vec(),
        SplitAxis::Horizontal => horizontal.to_vec(),
    }
}

fn other_current(baseline: &DragBaseline, vertical: &[i32], horizontal: &[i32]) -> Vec<i32> {
    match baseline.axis {
        SplitAxis::Vertical => horizontal.to_vec(),
        SplitAxis::Horizontal => vertical.to_vec(),
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

    /// Every literal string form `parse_action` accepts, paired with the
    /// exact [`TerminalAction`] it must produce. Together with the
    /// direction families and the `activate-tab-0..9` loop below this covers
    /// every reachable branch of the parser against the production action set
    /// in `src/registry.rs`, and each parsed action must still resolve to a
    /// managed bridge binding (the same lookup the dispatch test performs).
    #[test]
    fn parses_every_supported_probe_action() {
        let literal_cases: &[(&str, TerminalAction)] = &[
            ("new-tab", TerminalAction::NewTab),
            ("next-tab", TerminalAction::NextTab),
            ("previous-tab", TerminalAction::PreviousTab),
            ("close-pane", TerminalAction::ClosePane),
            ("toggle-zoom", TerminalAction::TogglePaneZoom),
            (
                "split-left",
                TerminalAction::SplitPane {
                    direction: Direction::Left,
                },
            ),
            (
                "split-right",
                TerminalAction::SplitPane {
                    direction: Direction::Right,
                },
            ),
            (
                "split-up",
                TerminalAction::SplitPane {
                    direction: Direction::Up,
                },
            ),
            (
                "split-down",
                TerminalAction::SplitPane {
                    direction: Direction::Down,
                },
            ),
            (
                "focus-left",
                TerminalAction::FocusPane {
                    direction: Direction::Left,
                },
            ),
            (
                "focus-right",
                TerminalAction::FocusPane {
                    direction: Direction::Right,
                },
            ),
            (
                "focus-up",
                TerminalAction::FocusPane {
                    direction: Direction::Up,
                },
            ),
            (
                "focus-down",
                TerminalAction::FocusPane {
                    direction: Direction::Down,
                },
            ),
            (
                "resize-left",
                TerminalAction::ResizePane {
                    direction: Direction::Left,
                },
            ),
            (
                "resize-right",
                TerminalAction::ResizePane {
                    direction: Direction::Right,
                },
            ),
            (
                "resize-up",
                TerminalAction::ResizePane {
                    direction: Direction::Up,
                },
            ),
            (
                "resize-down",
                TerminalAction::ResizePane {
                    direction: Direction::Down,
                },
            ),
        ];
        for &(input, expected) in literal_cases {
            assert_eq!(parse_action(input), Some(expected), "input {input:?}");
            assert!(
                binding_for_action(expected).is_some(),
                "{input:?} must resolve to a managed bridge binding"
            );
        }
        for index in 0..=9_u8 {
            let expected = TerminalAction::ActivateTab { index };
            let input = format!("activate-tab-{index}");
            assert_eq!(parse_action(&input), Some(expected), "input {input:?}");
            assert!(
                binding_for_action(expected).is_some(),
                "{input:?} must resolve to a managed bridge binding"
            );
        }
    }

    /// Pins every fall-through branch of `parse_action`: the direction probe
    /// after `split_once('-')`, the `activate-tab-` strip_prefix fallback, and
    /// the registry actions this env-var probe deliberately does not expose.
    #[test]
    fn rejects_unsupported_probe_action_forms() {
        for input in [
            // Direction probe: the prefix matches but the suffix is not a
            // bare left/right/up/down direction.
            "split-diagonal",
            "focus-pane-left",
            "rename-tab",
            // `activate-tab-` prefix fallback: index missing, non-numeric, or
            // outside the supported 0..=9 range.
            "activate-tab-",
            "activate-tab-x",
            "activate-tab-10",
            // No hyphen at all: the name falls through the strip_prefix
            // fallback untouched ("zoom" is the registry name suffix; only
            // "toggle-zoom" is accepted above).
            "newtab",
            "zoom",
            // Registry actions without a probe string form: the bridge keeps
            // them installed, and shutdown is never bridged at all
            // (ActionCommand::Shutdown in src/registry.rs).
            "send-prefix-literal",
            "shutdown",
        ] {
            assert_eq!(parse_action(input), None, "{input:?} must be rejected");
        }
    }
}
