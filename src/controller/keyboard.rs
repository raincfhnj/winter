use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;

use crate::model::{Direction, WindowIdentity};
use crate::platform::windows::{KeyTransition as RawTransition, RawKeyEvent, key_is_down};
use crate::prefix::{KeyEvent, KeyTransition, LogicalKey, Modifiers, PhysicalKey};

#[derive(Default)]
pub(super) struct KeyboardNormalizer {
    left_ctrl: bool,
    right_ctrl: bool,
    left_alt: bool,
    right_alt: bool,
    left_shift: bool,
    right_shift: bool,
    left_windows: bool,
    right_windows: bool,
}

impl KeyboardNormalizer {
    pub(super) fn normalize(
        &mut self,
        raw: RawKeyEvent,
        foreground_terminal: Option<WindowIdentity>,
    ) -> KeyEvent {
        self.update_modifier(raw.virtual_key, raw.transition);
        let alt_from_context = raw.is_alt_down
            && !(is_alt_key(raw.virtual_key) && raw.transition == RawTransition::Up);

        KeyEvent {
            physical_key: PhysicalKey::new(raw.scan_code, raw.is_extended),
            logical_key: logical_key(raw.virtual_key),
            transition: match raw.transition {
                RawTransition::Down => KeyTransition::Down,
                RawTransition::Up => KeyTransition::Up,
            },
            modifiers: Modifiers::new(
                self.left_ctrl || self.right_ctrl,
                self.left_alt || self.right_alt || alt_from_context,
                self.left_shift || self.right_shift,
                self.left_windows || self.right_windows,
            ),
            injected: raw.injected,
            foreground_terminal,
        }
    }

    fn update_modifier(&mut self, virtual_key: u32, transition: RawTransition) {
        let is_down = transition == RawTransition::Down;
        match virtual_key {
            VK_CONTROL | VK_LCONTROL => self.left_ctrl = is_down,
            VK_RCONTROL => self.right_ctrl = is_down,
            VK_MENU | VK_LMENU => self.left_alt = is_down,
            VK_RMENU => self.right_alt = is_down,
            VK_SHIFT | VK_LSHIFT => self.left_shift = is_down,
            VK_RSHIFT => self.right_shift = is_down,
            VK_LWIN => self.left_windows = is_down,
            VK_RWIN => self.right_windows = is_down,
            _ => {}
        }
        if !is_down {
            self.reconcile_released_modifier(virtual_key);
        }
    }

    /// Clears both sides of a modifier group after an observed key-up when
    /// async keyboard state reports the whole group is released.
    ///
    /// A press observed as `VK_RCONTROL` whose release is reported as the
    /// generic `VK_CONTROL` would otherwise clear only the left slot and
    /// leave the right slot stuck on, ghosting `modifiers.ctrl` into the
    /// prefix machine. Async state can only be trusted to mean "definitely
    /// not held" right after an observed up event: if it still reports the
    /// key down, slots are kept as-is because the other side is genuinely
    /// still held and its own events recover it.
    fn reconcile_released_modifier(&mut self, virtual_key: u32) {
        match virtual_key {
            VK_CONTROL | VK_LCONTROL | VK_RCONTROL
                if !key_is_down(VIRTUAL_KEY(VK_CONTROL as u16)) =>
            {
                self.left_ctrl = false;
                self.right_ctrl = false;
            }
            VK_MENU | VK_LMENU | VK_RMENU if !key_is_down(VIRTUAL_KEY(VK_MENU as u16)) => {
                self.left_alt = false;
                self.right_alt = false;
            }
            VK_SHIFT | VK_LSHIFT | VK_RSHIFT if !key_is_down(VIRTUAL_KEY(VK_SHIFT as u16)) => {
                self.left_shift = false;
                self.right_shift = false;
            }
            _ => {}
        }
    }
}

const VK_TAB: u32 = 0x09;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_ESCAPE: u32 = 0x1b;
const VK_SPACE: u32 = 0x20;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_F1: u32 = 0x70;
const VK_F12: u32 = 0x7b;
const VK_LSHIFT: u32 = 0xa0;
const VK_RSHIFT: u32 = 0xa1;
const VK_LCONTROL: u32 = 0xa2;
const VK_RCONTROL: u32 = 0xa3;
const VK_LMENU: u32 = 0xa4;
const VK_RMENU: u32 = 0xa5;
const VK_LWIN: u32 = 0x5b;
const VK_RWIN: u32 = 0x5c;
const VK_OEM_SEMICOLON: u32 = 0xba;
const VK_OEM_EQUALS: u32 = 0xbb;
const VK_OEM_COMMA: u32 = 0xbc;
const VK_OEM_MINUS: u32 = 0xbd;
const VK_OEM_PERIOD: u32 = 0xbe;
const VK_OEM_SLASH: u32 = 0xbf;
const VK_OEM_BACKTICK: u32 = 0xc0;
const VK_OEM_LEFT_BRACKET: u32 = 0xdb;
const VK_OEM_BACKSLASH: u32 = 0xdc;
const VK_OEM_RIGHT_BRACKET: u32 = 0xdd;
const VK_OEM_QUOTE: u32 = 0xde;

fn is_alt_key(virtual_key: u32) -> bool {
    matches!(virtual_key, VK_MENU | VK_LMENU | VK_RMENU)
}

fn logical_key(virtual_key: u32) -> LogicalKey {
    match virtual_key {
        VK_LEFT => LogicalKey::Arrow(Direction::Left),
        VK_RIGHT => LogicalKey::Arrow(Direction::Right),
        VK_UP => LogicalKey::Arrow(Direction::Up),
        VK_DOWN => LogicalKey::Arrow(Direction::Down),
        VK_ESCAPE => LogicalKey::Escape,
        VK_TAB => LogicalKey::Tab,
        VK_SPACE => LogicalKey::Character(' '),
        value @ VK_F1..=VK_F12 => LogicalKey::Function((value - VK_F1 + 1) as u8),
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL
        | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN => LogicalKey::Modifier,
        VK_OEM_SEMICOLON => LogicalKey::Character(';'),
        VK_OEM_EQUALS => LogicalKey::Character('='),
        VK_OEM_COMMA => LogicalKey::Character(','),
        VK_OEM_MINUS => LogicalKey::Character('-'),
        VK_OEM_PERIOD => LogicalKey::Character('.'),
        VK_OEM_SLASH => LogicalKey::Character('/'),
        VK_OEM_BACKTICK => LogicalKey::Character('`'),
        VK_OEM_LEFT_BRACKET => LogicalKey::Character('['),
        VK_OEM_BACKSLASH => LogicalKey::Character('\\'),
        VK_OEM_RIGHT_BRACKET => LogicalKey::Character(']'),
        VK_OEM_QUOTE => LogicalKey::Character('\''),
        value @ 0x30..=0x39 => LogicalKey::Character((value as u8) as char),
        value @ 0x41..=0x5a => LogicalKey::Character(((value as u8) + (b'a' - b'A')) as char),
        _ => LogicalKey::Other,
    }
}

/// Reverse of [`logical_key`] for the keys a Prefix chord may use.
///
/// Returns `None` for keys that cannot be injected (modifiers, Escape, and
/// layout-specific keys) so callers fail closed instead of guessing.
pub(super) fn virtual_key_for_logical_key(key: LogicalKey) -> Option<u16> {
    Some(match key {
        LogicalKey::Character(' ') => VK_SPACE as u16,
        LogicalKey::Character(character @ 'a'..='z') => u16::from(character as u8 - b'a' + b'A'),
        LogicalKey::Character(character @ '0'..='9') => u16::from(character as u8),
        LogicalKey::Character(';') => VK_OEM_SEMICOLON as u16,
        LogicalKey::Character('=') => VK_OEM_EQUALS as u16,
        LogicalKey::Character(',') => VK_OEM_COMMA as u16,
        LogicalKey::Character('-') => VK_OEM_MINUS as u16,
        LogicalKey::Character('.') => VK_OEM_PERIOD as u16,
        LogicalKey::Character('/') => VK_OEM_SLASH as u16,
        LogicalKey::Character('`') => VK_OEM_BACKTICK as u16,
        LogicalKey::Character('[') => VK_OEM_LEFT_BRACKET as u16,
        LogicalKey::Character('\\') => VK_OEM_BACKSLASH as u16,
        LogicalKey::Character(']') => VK_OEM_RIGHT_BRACKET as u16,
        LogicalKey::Character('\'') => VK_OEM_QUOTE as u16,
        LogicalKey::Arrow(Direction::Left) => VK_LEFT as u16,
        LogicalKey::Arrow(Direction::Right) => VK_RIGHT as u16,
        LogicalKey::Arrow(Direction::Up) => VK_UP as u16,
        LogicalKey::Arrow(Direction::Down) => VK_DOWN as u16,
        LogicalKey::Tab => VK_TAB as u16,
        LogicalKey::Function(number @ 1..=12) => VK_F1 as u16 + u16::from(number) - 1,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(virtual_key: u32, transition: RawTransition) -> RawKeyEvent {
        RawKeyEvent {
            virtual_key,
            scan_code: virtual_key,
            transition,
            is_system_key: false,
            is_extended: false,
            is_alt_down: false,
            injected: false,
            timestamp_ms: 0,
        }
    }

    /// Tracks observed ctrl presses through plain keystrokes and recovers
    /// both slots when a right-ctrl press is released as generic `VK_CONTROL`
    /// (bug M2: the release used to clear only the left slot and leave
    /// `modifiers.ctrl` stuck on).
    #[test]
    fn tracks_ctrl_state_and_recovers_stuck_slot_on_release() {
        let mut normalizer = KeyboardNormalizer::default();
        let ctrl = normalizer.normalize(raw(VK_LCONTROL, RawTransition::Down), None);
        assert!(ctrl.modifiers.ctrl);

        let b = normalizer.normalize(raw(u32::from(b'B'), RawTransition::Down), None);
        assert_eq!(b.logical_key, LogicalKey::Character('b'));
        assert!(b.modifiers.ctrl);

        let released = normalizer.normalize(raw(VK_LCONTROL, RawTransition::Up), None);
        assert!(!released.modifiers.ctrl);

        let right_down = normalizer.normalize(raw(VK_RCONTROL, RawTransition::Down), None);
        assert!(right_down.modifiers.ctrl);
        assert!(normalizer.right_ctrl);

        let mismatched_up = normalizer.normalize(raw(VK_CONTROL, RawTransition::Up), None);
        assert!(!mismatched_up.modifiers.ctrl);
        assert!(!normalizer.left_ctrl);
        assert!(!normalizer.right_ctrl);
    }

    #[test]
    fn generic_control_down_still_sets_left_slot_only() {
        let mut normalizer = KeyboardNormalizer::default();
        let ctrl = normalizer.normalize(raw(VK_CONTROL, RawTransition::Down), None);
        assert!(ctrl.modifiers.ctrl);
        assert!(normalizer.left_ctrl);
        assert!(!normalizer.right_ctrl);

        let shift = normalizer.normalize(raw(VK_SHIFT, RawTransition::Down), None);
        assert!(shift.modifiers.shift);
        assert!(normalizer.left_shift);
        assert!(!normalizer.right_shift);

        let alt = normalizer.normalize(raw(VK_MENU, RawTransition::Down), None);
        assert!(alt.modifiers.alt);
        assert!(normalizer.left_alt);
        assert!(!normalizer.right_alt);
    }

    #[test]
    fn generic_shift_release_clears_both_shift_slots() {
        let mut normalizer = KeyboardNormalizer::default();
        let right_down = normalizer.normalize(raw(VK_RSHIFT, RawTransition::Down), None);
        assert!(right_down.modifiers.shift);
        assert!(normalizer.right_shift);

        let mismatched_up = normalizer.normalize(raw(VK_SHIFT, RawTransition::Up), None);
        assert!(!mismatched_up.modifiers.shift);
        assert!(!normalizer.left_shift);
        assert!(!normalizer.right_shift);
    }

    #[test]
    fn generic_alt_release_clears_both_alt_slots() {
        let mut normalizer = KeyboardNormalizer::default();
        let right_down = normalizer.normalize(raw(VK_RMENU, RawTransition::Down), None);
        assert!(right_down.modifiers.alt);
        assert!(normalizer.right_alt);

        let mismatched_up = normalizer.normalize(raw(VK_MENU, RawTransition::Up), None);
        assert!(!mismatched_up.modifiers.alt);
        assert!(!normalizer.left_alt);
        assert!(!normalizer.right_alt);
    }

    #[test]
    fn is_alt_down_stays_authoritative_over_slot_state() {
        let mut normalizer = KeyboardNormalizer::default();
        let mut event = raw(u32::from(b'A'), RawTransition::Down);
        event.is_alt_down = true;
        let key = normalizer.normalize(event, None);
        assert!(key.modifiers.alt);
        assert!(!normalizer.left_alt);
        assert!(!normalizer.right_alt);
    }

    /// Documents the hook-restart edge (L9): the first event seen after a
    /// (re)start is a lone modifier Up with async state already clear. The
    /// slots must end up clear (recovery), never stuck — both when no press
    /// was ever observed by this normalizer and when a stale slot survived
    /// from before the restart.
    #[test]
    fn lone_modifier_up_as_first_event_after_restart_ends_clear() {
        let mut fresh = KeyboardNormalizer::default();
        let ctrl_up = fresh.normalize(raw(VK_RCONTROL, RawTransition::Up), None);
        assert!(!ctrl_up.modifiers.ctrl);
        assert!(!fresh.left_ctrl);
        assert!(!fresh.right_ctrl);

        let mut stale = KeyboardNormalizer::default();
        stale.normalize(raw(VK_RCONTROL, RawTransition::Down), None);
        assert!(stale.right_ctrl);

        let recovered = stale.normalize(raw(VK_CONTROL, RawTransition::Up), None);
        assert!(!recovered.modifiers.ctrl);
        assert!(!stale.left_ctrl);
        assert!(!stale.right_ctrl);
    }

    #[test]
    fn maps_direction_and_command_keys() {
        assert_eq!(logical_key(VK_LEFT), LogicalKey::Arrow(Direction::Left));
        assert_eq!(logical_key(VK_OEM_COMMA), LogicalKey::Character(','));
        assert_eq!(logical_key(VK_SPACE), LogicalKey::Character(' '));
        assert_eq!(logical_key(VK_F12), LogicalKey::Function(12));
        assert_eq!(logical_key(u32::from(b'9')), LogicalKey::Character('9'));
        assert_eq!(logical_key(u32::from(b'Q')), LogicalKey::Character('q'));
    }

    #[test]
    fn reverses_logical_keys_for_literal_prefix_replay() {
        assert_eq!(
            virtual_key_for_logical_key(LogicalKey::Character('b')),
            Some(u32::from(b'B') as u16)
        );
        assert_eq!(
            virtual_key_for_logical_key(LogicalKey::Arrow(Direction::Left)),
            Some(VK_LEFT as u16)
        );
        assert_eq!(
            virtual_key_for_logical_key(LogicalKey::Function(12)),
            Some(VK_F12 as u16)
        );
        assert_eq!(virtual_key_for_logical_key(LogicalKey::Escape), None);
        assert_eq!(virtual_key_for_logical_key(LogicalKey::Modifier), None);
    }
}
