use std::mem::size_of;

use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_F1, VK_LCONTROL, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

use crate::keymap::BridgeChord;
use crate::model::WindowIdentity;

use super::error::{ModifierKey, PlatformError, PlatformResult};
use super::foreground::{foreground_hwnd, validate_window_identity};
use super::hook::CONTROLLER_INPUT_MARKER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDispatch {
    pub sent: u32,
}

/// Sends a managed Windows Terminal bridge chord to an unchanged foreground
/// target.
///
/// This function never activates a window. Checks run in order of increasing
/// volatility — plan, identity, foreground, modifier state — so the most
/// volatile facts are verified last: HWND, PID, process creation time, and
/// channel are validated first, then the foreground HWND is re-checked, then
/// the physical modifier and target-key state is re-read against what the
/// plan assumed. Modifiers already held by the user are never released; any
/// state drift fails closed before `SendInput`.
pub fn send_bridge_chord(
    target: WindowIdentity,
    chord: BridgeChord,
) -> PlatformResult<InputDispatch> {
    let virtual_key = function_virtual_key(chord.function_key)?;
    send_key_chord(target, virtual_key, chord.ctrl, chord.alt, chord.shift)
}

/// Sends one arbitrary key chord to an unchanged foreground target.
///
/// Used for hidden bridge function keys. Only modifiers this call synthesizes
/// are released; modifiers already held by the user are preserved, extra held
/// modifiers fail closed, and a target key that is already physically held is
/// rejected before any input is injected and re-checked immediately before
/// `SendInput`.
fn send_key_chord(
    target: WindowIdentity,
    virtual_key: VIRTUAL_KEY,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> PlatformResult<InputDispatch> {
    send_chord(target, virtual_key, ctrl, alt, shift, true)
}

/// Sends one chord even when its target key is already physically held.
///
/// Literal prefix replay is triggered by the key-down of the very key it
/// re-injects, so the normal "target key already held" guard would always fail.
/// That physical key-down and its matching key-up are consumed by the
/// controller before reaching the target, so the injected chord is still the
/// only complete key transition the target observes. The target-key guard is
/// the only check relaxed; identity, foreground, and modifier re-checks still
/// run before `SendInput`.
pub fn send_literal_chord(
    target: WindowIdentity,
    virtual_key: VIRTUAL_KEY,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> PlatformResult<InputDispatch> {
    send_chord(target, virtual_key, ctrl, alt, shift, false)
}

/// Shared dispatch path for bridge and literal chords.
///
/// Checks run from slow/stable to fast/volatile: plan, identity validation,
/// foreground HWND, then physical modifier state — each re-verified after the
/// preceding slow step so the most volatile facts are confirmed immediately
/// before `SendInput`. A change occurring after the final re-check can still
/// race the injection itself; that residual window is inherent to user-mode
/// `SendInput`.
fn send_chord(
    target: WindowIdentity,
    virtual_key: VIRTUAL_KEY,
    ctrl: bool,
    alt: bool,
    shift: bool,
    check_target_key: bool,
) -> PlatformResult<InputDispatch> {
    let snapshot = ModifierSnapshot::capture(virtual_key);
    let plan = plan_chord_events(virtual_key, ctrl, alt, shift, snapshot, check_target_key)?;

    validate_window_identity(target)?;

    let actual_foreground = foreground_hwnd();
    if actual_foreground != target.hwnd {
        return Err(PlatformError::TargetNotForeground {
            expected: target.hwnd,
            actual: actual_foreground,
        });
    }

    recheck_modifier_state(virtual_key, snapshot, check_target_key)?;

    let inputs = plan.to_inputs();
    let expected = input_count(inputs.len())?;
    let input_size = input_structure_size()?;
    // SAFETY: setting and immediately reading the calling thread's last-error
    // value does not dereference pointers or transfer ownership.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    // SAFETY: `inputs` is a live contiguous INPUT slice and `input_size` is the
    // checked size of INPUT expected by SendInput.
    let sent = unsafe { SendInput(&inputs, input_size) };
    if sent != expected {
        // SAFETY: this reads the calling thread's last-error value immediately
        // after the failed/partial SendInput call.
        let os_error = unsafe { GetLastError() }.0;
        let cleanup = plan.cleanup_after(sent as usize);
        let cleanup_inputs = events_to_inputs(&cleanup);
        let cleanup_expected = input_count(cleanup_inputs.len())?;
        let cleanup_sent = if cleanup_inputs.is_empty() {
            0
        } else {
            // SAFETY: this only resets the calling thread's last-error value.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            // SAFETY: `cleanup_inputs` is a live contiguous INPUT slice and the
            // structure size was checked above.
            unsafe { SendInput(&cleanup_inputs, input_size) }
        };

        return Err(PlatformError::InputInjectionIncomplete {
            sent,
            expected,
            cleanup_sent,
            cleanup_expected,
            os_error,
        });
    }

    Ok(InputDispatch { sent })
}

fn input_count(count: usize) -> PlatformResult<u32> {
    u32::try_from(count).map_err(|_| PlatformError::InputSequenceTooLarge(count))
}

fn input_structure_size() -> PlatformResult<i32> {
    let size = size_of::<INPUT>();
    i32::try_from(size).map_err(|_| PlatformError::InputStructureSizeTooLarge(size))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ModifierSnapshot {
    control: bool,
    alt: bool,
    shift: bool,
    windows: bool,
    target_key: bool,
}

impl ModifierSnapshot {
    fn capture(target_key: VIRTUAL_KEY) -> Self {
        Self {
            control: key_is_down(VK_CONTROL),
            alt: key_is_down(VK_MENU),
            shift: key_is_down(VK_SHIFT),
            windows: key_is_down(VK_LWIN) || key_is_down(VK_RWIN),
            target_key: key_is_down(target_key),
        }
    }
}

/// Re-reads the physical keyboard state and fails closed when it drifted from
/// the snapshot the input plan was built on.
fn recheck_modifier_state(
    virtual_key: VIRTUAL_KEY,
    planned: ModifierSnapshot,
    check_target_key: bool,
) -> PlatformResult<()> {
    let fresh = ModifierSnapshot::capture(virtual_key);
    compare_modifier_state(virtual_key, planned, fresh, check_target_key)
}

/// Decides whether `fresh` still matches every assumption `plan` was built on.
///
/// A modifier that was not held when the plan was made and is held now would
/// be clobbered or leak into the chord, so it fails with
/// [`PlatformError::UnexpectedModifierHeld`]. A modifier the plan relied on
/// being held (no down event was planned for it) that is now released fails
/// with [`PlatformError::ModifierReleased`]. The target key is only compared
/// when the plan requires it to be free (`check_target_key`), because literal
/// replay intentionally allows it to be held.
fn compare_modifier_state(
    virtual_key: VIRTUAL_KEY,
    planned: ModifierSnapshot,
    fresh: ModifierSnapshot,
    check_target_key: bool,
) -> PlatformResult<()> {
    for (was, now, name) in [
        (planned.control, fresh.control, ModifierKey::Control),
        (planned.alt, fresh.alt, ModifierKey::Alt),
        (planned.shift, fresh.shift, ModifierKey::Shift),
        (planned.windows, fresh.windows, ModifierKey::Windows),
    ] {
        if was && !now {
            return Err(PlatformError::ModifierReleased(name));
        }
        if !was && now {
            return Err(PlatformError::UnexpectedModifierHeld(name));
        }
    }
    if check_target_key && fresh.target_key {
        return Err(PlatformError::TargetKeyHeld(virtual_key.0));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlannedKeyEvent {
    virtual_key: VIRTUAL_KEY,
    transition: KeyTransition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyTransition {
    Down,
    Up,
}

#[derive(Debug, PartialEq, Eq)]
struct InputPlan {
    events: Vec<PlannedKeyEvent>,
}

impl InputPlan {
    fn to_inputs(&self) -> Vec<INPUT> {
        events_to_inputs(&self.events)
    }

    fn cleanup_after(&self, sent: usize) -> Vec<PlannedKeyEvent> {
        let mut down = Vec::with_capacity(4);
        for event in self.events.iter().take(sent.min(self.events.len())) {
            match event.transition {
                KeyTransition::Down => {
                    if !down.contains(&event.virtual_key) {
                        down.push(event.virtual_key);
                    }
                }
                KeyTransition::Up => {
                    if let Some(index) = down
                        .iter()
                        .rposition(|virtual_key| *virtual_key == event.virtual_key)
                    {
                        down.remove(index);
                    }
                }
            }
        }

        down.into_iter()
            .rev()
            .map(|virtual_key| PlannedKeyEvent {
                virtual_key,
                transition: KeyTransition::Up,
            })
            .collect()
    }
}

fn plan_chord_events(
    virtual_key: VIRTUAL_KEY,
    ctrl: bool,
    alt: bool,
    shift: bool,
    snapshot: ModifierSnapshot,
    check_target_key: bool,
) -> PlatformResult<InputPlan> {
    if check_target_key && snapshot.target_key {
        return Err(PlatformError::TargetKeyHeld(virtual_key.0));
    }
    if snapshot.windows {
        return Err(PlatformError::UnexpectedModifierHeld(ModifierKey::Windows));
    }

    let mut events = Vec::with_capacity(8);
    let mut synthesized = Vec::with_capacity(3);
    plan_modifier(
        ctrl,
        snapshot.control,
        ModifierKey::Control,
        VK_LCONTROL,
        &mut events,
        &mut synthesized,
    )?;
    plan_modifier(
        alt,
        snapshot.alt,
        ModifierKey::Alt,
        VK_LMENU,
        &mut events,
        &mut synthesized,
    )?;
    plan_modifier(
        shift,
        snapshot.shift,
        ModifierKey::Shift,
        VK_LSHIFT,
        &mut events,
        &mut synthesized,
    )?;

    events.push(PlannedKeyEvent {
        virtual_key,
        transition: KeyTransition::Down,
    });
    events.push(PlannedKeyEvent {
        virtual_key,
        transition: KeyTransition::Up,
    });
    events.extend(
        synthesized
            .iter()
            .rev()
            .copied()
            .map(|virtual_key| PlannedKeyEvent {
                virtual_key,
                transition: KeyTransition::Up,
            }),
    );

    Ok(InputPlan { events })
}

fn plan_modifier(
    required: bool,
    held: bool,
    name: ModifierKey,
    virtual_key: VIRTUAL_KEY,
    events: &mut Vec<PlannedKeyEvent>,
    synthesized: &mut Vec<VIRTUAL_KEY>,
) -> PlatformResult<()> {
    match (required, held) {
        (false, true) => Err(PlatformError::UnexpectedModifierHeld(name)),
        (true, false) => {
            events.push(PlannedKeyEvent {
                virtual_key,
                transition: KeyTransition::Down,
            });
            synthesized.push(virtual_key);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn function_virtual_key(function_key: u8) -> PlatformResult<VIRTUAL_KEY> {
    if !(13..=24).contains(&function_key) {
        return Err(PlatformError::InvalidBridgeFunctionKey(function_key));
    }

    Ok(VIRTUAL_KEY(VK_F1.0 + u16::from(function_key - 1)))
}

pub(crate) fn key_is_down(virtual_key: VIRTUAL_KEY) -> bool {
    // SAFETY: GetAsyncKeyState accepts any virtual-key code by value and has no
    // pointer or ownership requirements.
    unsafe { GetAsyncKeyState(i32::from(virtual_key.0)) < 0 }
}

fn events_to_inputs(events: &[PlannedKeyEvent]) -> Vec<INPUT> {
    events.iter().copied().map(event_to_input).collect()
}

fn event_to_input(event: PlannedKeyEvent) -> INPUT {
    let flags = match event.transition {
        KeyTransition::Down => KEYBD_EVENT_FLAGS::default(),
        KeyTransition::Up => KEYEVENTF_KEYUP,
    };

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: event.virtual_key,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: CONTROLLER_INPUT_MARKER,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fkey(function_key: u8) -> VIRTUAL_KEY {
        function_virtual_key(function_key).expect("fixture function key should be valid")
    }

    #[test]
    fn validates_bridge_function_key_range() {
        assert!(matches!(
            function_virtual_key(12),
            Err(PlatformError::InvalidBridgeFunctionKey(12))
        ));
        assert_eq!(function_virtual_key(13).expect("F13").0, 0x7c);
        assert_eq!(function_virtual_key(24).expect("F24").0, 0x87);
    }

    #[test]
    fn only_releases_modifiers_synthesized_by_this_dispatch() {
        let plan = plan_chord_events(
            fkey(13),
            true,
            true,
            false,
            ModifierSnapshot {
                control: true,
                ..ModifierSnapshot::default()
            },
            true,
        )
        .expect("valid plan");

        assert_eq!(
            plan.events,
            vec![
                PlannedKeyEvent {
                    virtual_key: VK_LMENU,
                    transition: KeyTransition::Down,
                },
                PlannedKeyEvent {
                    virtual_key: VIRTUAL_KEY(0x7c),
                    transition: KeyTransition::Down,
                },
                PlannedKeyEvent {
                    virtual_key: VIRTUAL_KEY(0x7c),
                    transition: KeyTransition::Up,
                },
                PlannedKeyEvent {
                    virtual_key: VK_LMENU,
                    transition: KeyTransition::Up,
                },
            ]
        );
    }

    #[test]
    fn plans_a_literal_character_chord() {
        let plan = plan_chord_events(
            VIRTUAL_KEY(u16::from(b'B')),
            true,
            false,
            false,
            ModifierSnapshot::default(),
            false,
        )
        .expect("valid plan");

        assert_eq!(
            plan.events,
            vec![
                PlannedKeyEvent {
                    virtual_key: VK_LCONTROL,
                    transition: KeyTransition::Down,
                },
                PlannedKeyEvent {
                    virtual_key: VIRTUAL_KEY(u16::from(b'B')),
                    transition: KeyTransition::Down,
                },
                PlannedKeyEvent {
                    virtual_key: VIRTUAL_KEY(u16::from(b'B')),
                    transition: KeyTransition::Up,
                },
                PlannedKeyEvent {
                    virtual_key: VK_LCONTROL,
                    transition: KeyTransition::Up,
                },
            ]
        );
    }

    #[test]
    fn literal_replay_allows_a_physically_held_target_key() {
        let held = ModifierSnapshot {
            target_key: true,
            ..ModifierSnapshot::default()
        };

        assert!(matches!(
            plan_chord_events(VIRTUAL_KEY(u16::from(b'B')), true, false, false, held, true),
            Err(PlatformError::TargetKeyHeld(_))
        ));
        assert!(
            plan_chord_events(
                VIRTUAL_KEY(u16::from(b'B')),
                true,
                false,
                false,
                held,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn refuses_extra_physical_modifier() {
        assert!(matches!(
            plan_chord_events(
                fkey(13),
                false,
                false,
                false,
                ModifierSnapshot {
                    shift: true,
                    ..ModifierSnapshot::default()
                },
                true
            ),
            Err(PlatformError::UnexpectedModifierHeld(ModifierKey::Shift))
        ));
    }

    #[test]
    fn modifier_recheck_accepts_unchanged_state() {
        let planned = ModifierSnapshot {
            control: true,
            shift: true,
            ..ModifierSnapshot::default()
        };

        assert!(compare_modifier_state(fkey(13), planned, planned, true).is_ok());
        assert!(
            compare_modifier_state(
                fkey(13),
                ModifierSnapshot::default(),
                ModifierSnapshot::default(),
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn modifier_recheck_reports_newly_held_modifier() {
        let fresh = ModifierSnapshot {
            control: true,
            ..ModifierSnapshot::default()
        };

        assert!(matches!(
            compare_modifier_state(fkey(13), ModifierSnapshot::default(), fresh, true),
            Err(PlatformError::UnexpectedModifierHeld(ModifierKey::Control))
        ));
    }

    #[test]
    fn modifier_recheck_reports_newly_held_windows_key() {
        let fresh = ModifierSnapshot {
            windows: true,
            ..ModifierSnapshot::default()
        };

        assert!(matches!(
            compare_modifier_state(fkey(13), ModifierSnapshot::default(), fresh, true),
            Err(PlatformError::UnexpectedModifierHeld(ModifierKey::Windows))
        ));
    }

    #[test]
    fn modifier_recheck_reports_released_expected_modifier() {
        let planned = ModifierSnapshot {
            alt: true,
            ..ModifierSnapshot::default()
        };

        assert!(matches!(
            compare_modifier_state(fkey(13), planned, ModifierSnapshot::default(), true),
            Err(PlatformError::ModifierReleased(ModifierKey::Alt))
        ));
    }

    #[test]
    fn modifier_recheck_reports_target_key_pressed_after_plan() {
        let fresh = ModifierSnapshot {
            target_key: true,
            ..ModifierSnapshot::default()
        };

        assert!(matches!(
            compare_modifier_state(
                VIRTUAL_KEY(u16::from(b'B')),
                ModifierSnapshot::default(),
                fresh,
                true
            ),
            Err(PlatformError::TargetKeyHeld(_))
        ));
        assert!(
            compare_modifier_state(
                VIRTUAL_KEY(u16::from(b'B')),
                ModifierSnapshot::default(),
                fresh,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn cleanup_releases_only_keys_left_down_by_partial_insert() {
        let plan = plan_chord_events(
            fkey(13),
            true,
            true,
            false,
            ModifierSnapshot::default(),
            true,
        )
        .expect("valid plan");

        assert_eq!(
            plan.cleanup_after(3),
            vec![
                PlannedKeyEvent {
                    virtual_key: VIRTUAL_KEY(0x7c),
                    transition: KeyTransition::Up,
                },
                PlannedKeyEvent {
                    virtual_key: VK_LMENU,
                    transition: KeyTransition::Up,
                },
                PlannedKeyEvent {
                    virtual_key: VK_LCONTROL,
                    transition: KeyTransition::Up,
                },
            ]
        );
        assert!(plan.cleanup_after(plan.events.len()).is_empty());
    }

    #[test]
    fn every_input_carries_controller_marker() {
        let plan = plan_chord_events(
            fkey(24),
            false,
            false,
            false,
            ModifierSnapshot::default(),
            true,
        )
        .expect("valid plan");

        for input in plan.to_inputs() {
            // SAFETY: event_to_input initialized the active union member as a
            // keyboard INPUT, so reading `ki` is valid in this test.
            let keyboard = unsafe { input.Anonymous.ki };
            assert_eq!(keyboard.dwExtraInfo, CONTROLLER_INPUT_MARKER);
        }
    }
}
