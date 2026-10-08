//! Canonical registry of every configurable action.
//!
//! [`ACTIONS`] is the single source of truth for both manually-synced tables
//! this module replaces: the prefix shortcut specs
//! ([`crate::prefix::shortcut_specs`]) and the Windows Terminal bridge table
//! ([`crate::keymap::managed_bindings`]). Both views are derived from the
//! registry at compile time, so introducing a [`TerminalAction`] only requires
//! one registry entry. The const assertions at the bottom of this module
//! reject duplicate or incomplete registries while the crate compiles.

use crate::keymap::{BridgeChord, ManagedBinding, action_id_prefix};
use crate::model::{Direction, TerminalAction};
use crate::prefix::{KeyChord, LogicalKey, Modifiers, ShortcutCommand, ShortcutSpec, chord};

/// The Windows Terminal bridge identity attached to a managed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeBinding {
    /// Stable action id referenced by the fragment and keybindings arrays.
    pub action_id: &'static str,
    /// Synthetic chord Windows Terminal listens for on behalf of the action.
    pub chord: BridgeChord,
}

/// What fires when a registry entry's prefix chord is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCommand {
    /// A dispatchable terminal action together with its bridge binding.
    ///
    /// Every terminal action carries its bridge data inline, so an action
    /// without a Windows Terminal id and chord cannot be registered.
    Terminal {
        action: TerminalAction,
        bridge: BridgeBinding,
    },
    /// The controller shutdown command; it is never bridged and may not be
    /// disabled in configuration.
    Shutdown,
}

/// One configurable action: the config shortcut, its default chord, and its
/// Windows Terminal bridge binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionEntry {
    /// Stable shortcut name used as the configuration key and in diagnostics.
    pub name: &'static str,
    /// What the shortcut dispatches.
    pub command: ActionCommand,
    /// Default prefix chord for the shortcut.
    pub default_chord: KeyChord,
    /// Whether configuration may assign `disabled` to the shortcut.
    pub allow_disabled: bool,
}

impl ActionEntry {
    /// Whether this entry is the controller shutdown command.
    #[must_use]
    pub const fn is_shutdown(self) -> bool {
        matches!(self.command, ActionCommand::Shutdown)
    }

    /// The terminal action, or `None` for the shutdown command.
    #[must_use]
    pub const fn terminal_action(self) -> Option<TerminalAction> {
        match self.command {
            ActionCommand::Terminal { action, .. } => Some(action),
            ActionCommand::Shutdown => None,
        }
    }
}

const fn terminal_entry(
    name: &'static str,
    default_chord: KeyChord,
    action: TerminalAction,
    action_id: &'static str,
    bridge_chord: BridgeChord,
) -> ActionEntry {
    ActionEntry {
        name,
        command: ActionCommand::Terminal {
            action,
            bridge: BridgeBinding {
                action_id,
                chord: bridge_chord,
            },
        },
        default_chord,
        allow_disabled: true,
    }
}

const fn shutdown_entry(default_chord: KeyChord) -> ActionEntry {
    ActionEntry {
        name: "shutdown",
        command: ActionCommand::Shutdown,
        default_chord,
        allow_disabled: false,
    }
}

/// Every configurable action in canonical bridge-table order.
///
/// The derived bridge table keeps this order verbatim so installed fragments
/// and keybindings stay byte-identical; `SPEC_ORDER` restates the
/// user-facing shortcut order when the prefix specs are derived.
pub const ACTIONS: &[ActionEntry] = &[
    terminal_entry(
        "split_left",
        chord(LogicalKey::Arrow(Direction::Left), false, false, true),
        TerminalAction::SplitPane {
            direction: Direction::Left,
        },
        "User.Winter.SplitLeft",
        BridgeChord::new(true, true, true, 13),
    ),
    terminal_entry(
        "split_right",
        chord(LogicalKey::Arrow(Direction::Right), false, false, true),
        TerminalAction::SplitPane {
            direction: Direction::Right,
        },
        "User.Winter.SplitRight",
        BridgeChord::new(true, true, true, 14),
    ),
    terminal_entry(
        "split_up",
        chord(LogicalKey::Arrow(Direction::Up), false, false, true),
        TerminalAction::SplitPane {
            direction: Direction::Up,
        },
        "User.Winter.SplitUp",
        BridgeChord::new(true, true, true, 15),
    ),
    terminal_entry(
        "split_down",
        chord(LogicalKey::Arrow(Direction::Down), false, false, true),
        TerminalAction::SplitPane {
            direction: Direction::Down,
        },
        "User.Winter.SplitDown",
        BridgeChord::new(true, true, true, 18),
    ),
    terminal_entry(
        "focus_left",
        chord(LogicalKey::Arrow(Direction::Left), false, false, false),
        TerminalAction::FocusPane {
            direction: Direction::Left,
        },
        "User.Winter.FocusLeft",
        BridgeChord::new(true, true, true, 19),
    ),
    terminal_entry(
        "focus_right",
        chord(LogicalKey::Arrow(Direction::Right), false, false, false),
        TerminalAction::FocusPane {
            direction: Direction::Right,
        },
        "User.Winter.FocusRight",
        BridgeChord::new(true, true, true, 20),
    ),
    terminal_entry(
        "focus_up",
        chord(LogicalKey::Arrow(Direction::Up), false, false, false),
        TerminalAction::FocusPane {
            direction: Direction::Up,
        },
        "User.Winter.FocusUp",
        BridgeChord::new(true, true, true, 21),
    ),
    terminal_entry(
        "focus_down",
        chord(LogicalKey::Arrow(Direction::Down), false, false, false),
        TerminalAction::FocusPane {
            direction: Direction::Down,
        },
        "User.Winter.FocusDown",
        BridgeChord::new(true, true, true, 22),
    ),
    terminal_entry(
        "resize_left",
        chord(LogicalKey::Arrow(Direction::Left), true, false, false),
        TerminalAction::ResizePane {
            direction: Direction::Left,
        },
        "User.Winter.ResizeLeft",
        BridgeChord::new(true, true, true, 23),
    ),
    terminal_entry(
        "resize_right",
        chord(LogicalKey::Arrow(Direction::Right), true, false, false),
        TerminalAction::ResizePane {
            direction: Direction::Right,
        },
        "User.Winter.ResizeRight",
        BridgeChord::new(true, true, true, 24),
    ),
    terminal_entry(
        "resize_up",
        chord(LogicalKey::Arrow(Direction::Up), true, false, false),
        TerminalAction::ResizePane {
            direction: Direction::Up,
        },
        "User.Winter.ResizeUp",
        BridgeChord::new(true, false, true, 13),
    ),
    terminal_entry(
        "resize_down",
        chord(LogicalKey::Arrow(Direction::Down), true, false, false),
        TerminalAction::ResizePane {
            direction: Direction::Down,
        },
        "User.Winter.ResizeDown",
        BridgeChord::new(true, false, true, 14),
    ),
    // The controller now injects the configured Prefix directly, so this static
    // action and chord are never dispatched. They are retained so existing
    // installs keep a valid action reference and their managed count stays put.
    terminal_entry(
        "send_prefix_literal",
        chord(LogicalKey::Character('b'), false, false, false),
        TerminalAction::SendPrefixLiteral,
        "User.Winter.SendPrefixLiteral",
        BridgeChord::new(true, false, true, 15),
    ),
    terminal_entry(
        "new_tab",
        chord(LogicalKey::Character('c'), false, false, false),
        TerminalAction::NewTab,
        "User.Winter.NewTab",
        BridgeChord::new(true, false, true, 18),
    ),
    terminal_entry(
        "next_tab",
        chord(LogicalKey::Character('n'), false, false, false),
        TerminalAction::NextTab,
        "User.Winter.NextTab",
        BridgeChord::new(true, false, true, 19),
    ),
    terminal_entry(
        "previous_tab",
        chord(LogicalKey::Character('p'), false, false, false),
        TerminalAction::PreviousTab,
        "User.Winter.PreviousTab",
        BridgeChord::new(true, false, true, 20),
    ),
    terminal_entry(
        "activate_tab_0",
        chord(LogicalKey::Character('0'), false, false, false),
        TerminalAction::ActivateTab { index: 0 },
        "User.Winter.ActivateTab0",
        BridgeChord::new(true, false, true, 21),
    ),
    terminal_entry(
        "activate_tab_1",
        chord(LogicalKey::Character('1'), false, false, false),
        TerminalAction::ActivateTab { index: 1 },
        "User.Winter.ActivateTab1",
        BridgeChord::new(true, false, true, 22),
    ),
    terminal_entry(
        "activate_tab_2",
        chord(LogicalKey::Character('2'), false, false, false),
        TerminalAction::ActivateTab { index: 2 },
        "User.Winter.ActivateTab2",
        BridgeChord::new(true, false, true, 23),
    ),
    terminal_entry(
        "activate_tab_3",
        chord(LogicalKey::Character('3'), false, false, false),
        TerminalAction::ActivateTab { index: 3 },
        "User.Winter.ActivateTab3",
        BridgeChord::new(true, false, true, 24),
    ),
    terminal_entry(
        "activate_tab_4",
        chord(LogicalKey::Character('4'), false, false, false),
        TerminalAction::ActivateTab { index: 4 },
        "User.Winter.ActivateTab4",
        BridgeChord::new(false, true, true, 13),
    ),
    terminal_entry(
        "activate_tab_5",
        chord(LogicalKey::Character('5'), false, false, false),
        TerminalAction::ActivateTab { index: 5 },
        "User.Winter.ActivateTab5",
        BridgeChord::new(false, true, true, 14),
    ),
    terminal_entry(
        "activate_tab_6",
        chord(LogicalKey::Character('6'), false, false, false),
        TerminalAction::ActivateTab { index: 6 },
        "User.Winter.ActivateTab6",
        BridgeChord::new(false, true, true, 15),
    ),
    terminal_entry(
        "activate_tab_7",
        chord(LogicalKey::Character('7'), false, false, false),
        TerminalAction::ActivateTab { index: 7 },
        "User.Winter.ActivateTab7",
        BridgeChord::new(false, true, true, 18),
    ),
    terminal_entry(
        "activate_tab_8",
        chord(LogicalKey::Character('8'), false, false, false),
        TerminalAction::ActivateTab { index: 8 },
        "User.Winter.ActivateTab8",
        BridgeChord::new(false, true, true, 19),
    ),
    terminal_entry(
        "activate_tab_9",
        chord(LogicalKey::Character('9'), false, false, false),
        TerminalAction::ActivateTab { index: 9 },
        "User.Winter.ActivateTab9",
        BridgeChord::new(false, true, true, 20),
    ),
    terminal_entry(
        "close_pane",
        chord(LogicalKey::Character('x'), false, false, false),
        TerminalAction::ClosePane,
        "User.Winter.ClosePane",
        BridgeChord::new(false, true, true, 21),
    ),
    terminal_entry(
        "toggle_zoom",
        chord(LogicalKey::Character('z'), false, false, false),
        TerminalAction::TogglePaneZoom,
        "User.Winter.TogglePaneZoom",
        BridgeChord::new(false, true, true, 22),
    ),
    terminal_entry(
        "rename_tab",
        chord(LogicalKey::Character(','), false, false, false),
        TerminalAction::RenameTab,
        "User.Winter.RenameTab",
        BridgeChord::new(false, true, true, 23),
    ),
    shutdown_entry(chord(LogicalKey::Character('q'), false, false, false)),
];

/// Number of registry entries, including the shutdown command.
pub const ACTION_COUNT: usize = ACTIONS.len();

/// Number of registry entries that carry a Windows Terminal bridge binding.
pub const MANAGED_COUNT: usize = count_managed();

/// Registry indices in the user-facing shortcut order: focus, split, resize,
/// tab management, pane management, then shutdown. The const assertions below
/// prove it lists every registry index exactly once, so the derived shortcut
/// specs are always a complete permutation of [`ACTIONS`].
const SPEC_ORDER: [usize; ACTION_COUNT] = [
    4, 5, 6, 7, // focus_left..focus_down
    0, 1, 2, 3, // split_left..split_down
    8, 9, 10, 11, // resize_left..resize_down
    13, 14, 15, // new_tab, next_tab, previous_tab
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, // activate_tab_0..9
    26, 27, 28, // close_pane, toggle_zoom, rename_tab
    12, // send_prefix_literal
    29, // shutdown
];

const fn count_managed() -> usize {
    let mut count = 0;
    let mut index = 0;
    while index < ACTIONS.len() {
        if matches!(ACTIONS[index].command, ActionCommand::Terminal { .. }) {
            count += 1;
        }
        index += 1;
    }
    count
}

const fn count_shutdown() -> usize {
    let mut count = 0;
    let mut index = 0;
    while index < ACTIONS.len() {
        if ACTIONS[index].is_shutdown() {
            count += 1;
        }
        index += 1;
    }
    count
}

const fn spec_of(entry: ActionEntry) -> ShortcutSpec {
    ShortcutSpec {
        name: entry.name,
        default_chord: entry.default_chord,
        command: match entry.command {
            ActionCommand::Terminal { action, .. } => ShortcutCommand::Terminal(action),
            ActionCommand::Shutdown => ShortcutCommand::Shutdown,
        },
        allow_disabled: entry.allow_disabled,
    }
}

const fn binding_of(entry: ActionEntry) -> ManagedBinding {
    match entry.command {
        ActionCommand::Terminal { action, bridge } => ManagedBinding {
            action,
            action_id: bridge.action_id,
            bridge_chord: bridge.chord,
        },
        ActionCommand::Shutdown => panic!("shutdown entries never become managed bindings"),
    }
}

/// Derives the prefix shortcut table from [`ACTIONS`] in user-facing order.
pub(crate) const fn build_shortcut_specs() -> [ShortcutSpec; ACTION_COUNT] {
    let mut specs = [spec_of(ACTIONS[0]); ACTION_COUNT];
    let mut position = 0;
    while position < ACTION_COUNT {
        specs[position] = spec_of(ACTIONS[SPEC_ORDER[position]]);
        position += 1;
    }
    specs
}

/// Derives the Windows Terminal bridge table from [`ACTIONS`], preserving the
/// registry order and skipping the shutdown command.
pub(crate) const fn build_managed_bindings() -> [ManagedBinding; MANAGED_COUNT] {
    let mut seed = None;
    let mut index = 0;
    while index < ACTION_COUNT {
        if matches!(ACTIONS[index].command, ActionCommand::Terminal { .. }) {
            seed = Some(binding_of(ACTIONS[index]));
            break;
        }
        index += 1;
    }
    let Some(seed) = seed else {
        panic!("registry must contain at least one terminal action");
    };

    let mut bindings = [seed; MANAGED_COUNT];
    let mut written = 0;
    let mut index = 0;
    while index < ACTION_COUNT {
        if let ActionCommand::Terminal { .. } = ACTIONS[index].command {
            bindings[written] = binding_of(ACTIONS[index]);
            written += 1;
        }
        index += 1;
    }
    assert!(
        written == MANAGED_COUNT,
        "managed binding writer/counter mismatch"
    );
    bindings
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut index = 0;
    while index < a.len() {
        if a[index] != b[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn str_starts_with(value: &str, prefix: &str) -> bool {
    let (value, prefix) = (value.as_bytes(), prefix.as_bytes());
    if value.len() < prefix.len() {
        return false;
    }
    let mut index = 0;
    while index < prefix.len() {
        if value[index] != prefix[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn direction_eq(a: Direction, b: Direction) -> bool {
    matches!(
        (a, b),
        (Direction::Left, Direction::Left)
            | (Direction::Right, Direction::Right)
            | (Direction::Up, Direction::Up)
            | (Direction::Down, Direction::Down)
    )
}

const fn key_eq(a: LogicalKey, b: LogicalKey) -> bool {
    match (a, b) {
        (LogicalKey::Character(a), LogicalKey::Character(b)) => a == b,
        (LogicalKey::Arrow(a), LogicalKey::Arrow(b)) => direction_eq(a, b),
        (LogicalKey::Function(a), LogicalKey::Function(b)) => a == b,
        (LogicalKey::Escape, LogicalKey::Escape)
        | (LogicalKey::Tab, LogicalKey::Tab)
        | (LogicalKey::Modifier, LogicalKey::Modifier)
        | (LogicalKey::Other, LogicalKey::Other) => true,
        _ => false,
    }
}

const fn modifiers_eq(a: Modifiers, b: Modifiers) -> bool {
    a.ctrl == b.ctrl && a.alt == b.alt && a.shift == b.shift && a.windows == b.windows
}

const fn chord_eq(a: KeyChord, b: KeyChord) -> bool {
    key_eq(a.key, b.key) && modifiers_eq(a.modifiers, b.modifiers)
}

const fn bridge_chord_eq(a: BridgeChord, b: BridgeChord) -> bool {
    a.ctrl == b.ctrl && a.alt == b.alt && a.shift == b.shift && a.function_key == b.function_key
}

const fn action_eq(a: TerminalAction, b: TerminalAction) -> bool {
    match (a, b) {
        (
            TerminalAction::SplitPane { direction: a },
            TerminalAction::SplitPane { direction: b },
        )
        | (
            TerminalAction::FocusPane { direction: a },
            TerminalAction::FocusPane { direction: b },
        )
        | (
            TerminalAction::ResizePane { direction: a },
            TerminalAction::ResizePane { direction: b },
        ) => direction_eq(a, b),
        (TerminalAction::ActivateTab { index: a }, TerminalAction::ActivateTab { index: b }) => {
            a == b
        }
        (TerminalAction::SendPrefixLiteral, TerminalAction::SendPrefixLiteral)
        | (TerminalAction::NewTab, TerminalAction::NewTab)
        | (TerminalAction::NextTab, TerminalAction::NextTab)
        | (TerminalAction::PreviousTab, TerminalAction::PreviousTab)
        | (TerminalAction::ClosePane, TerminalAction::ClosePane)
        | (TerminalAction::TogglePaneZoom, TerminalAction::TogglePaneZoom)
        | (TerminalAction::RenameTab, TerminalAction::RenameTab) => true,
        _ => false,
    }
}

/// Compile-time consistency gate for the registry.
///
/// Adding, removing, or duplicating entries in a way that would desynchronize
/// the shortcut specs, the bridge table, or their counts fails compilation
/// here instead of surfacing as a runtime count assertion.
const _: () = {
    assert!(
        count_shutdown() == 1,
        "the registry must declare exactly one shutdown command"
    );
    assert!(
        MANAGED_COUNT > 0,
        "the registry must declare at least one terminal action"
    );

    let mut outer = 0;
    while outer < SPEC_ORDER.len() {
        assert!(
            SPEC_ORDER[outer] < ACTION_COUNT,
            "SPEC_ORDER indexes outside the registry"
        );
        let mut inner = outer + 1;
        while inner < SPEC_ORDER.len() {
            assert!(
                SPEC_ORDER[outer] != SPEC_ORDER[inner],
                "SPEC_ORDER must list every registry index exactly once"
            );
            inner += 1;
        }
        outer += 1;
    }

    let mut outer = 0;
    while outer < ACTION_COUNT {
        let mut inner = outer + 1;
        while inner < ACTION_COUNT {
            assert!(
                !str_eq(ACTIONS[outer].name, ACTIONS[inner].name),
                "shortcut names must be unique"
            );
            assert!(
                !chord_eq(ACTIONS[outer].default_chord, ACTIONS[inner].default_chord),
                "default chords must be unique"
            );
            inner += 1;
        }
        outer += 1;
    }

    let mut outer = 0;
    while outer < ACTION_COUNT {
        if let ActionCommand::Terminal { action, bridge } = ACTIONS[outer].command {
            assert!(
                str_starts_with(bridge.action_id, action_id_prefix()),
                "bridge action ids must carry the Winter prefix"
            );
            let mut inner = outer + 1;
            while inner < ACTION_COUNT {
                if let ActionCommand::Terminal {
                    action: other_action,
                    bridge: other_bridge,
                } = ACTIONS[inner].command
                {
                    assert!(
                        !action_eq(action, other_action),
                        "terminal actions must be unique"
                    );
                    assert!(
                        !str_eq(bridge.action_id, other_bridge.action_id),
                        "bridge action ids must be unique"
                    );
                    assert!(
                        !bridge_chord_eq(bridge.chord, other_bridge.chord),
                        "bridge chords must be unique"
                    );
                }
                inner += 1;
            }
        }
        outer += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{binding_for_action, managed_bindings};
    use crate::prefix::shortcut_specs;

    #[test]
    fn counts_partition_into_managed_and_shutdown_entries() {
        assert_eq!(shortcut_specs().len(), ACTION_COUNT);
        assert_eq!(managed_bindings().len(), MANAGED_COUNT);
        assert_eq!(MANAGED_COUNT + count_shutdown(), ACTION_COUNT);
        assert_eq!(
            shortcut_specs()
                .iter()
                .filter(|spec| matches!(spec.command, ShortcutCommand::Shutdown))
                .count(),
            count_shutdown()
        );
    }

    #[test]
    fn shortcut_specs_mirror_the_registry_in_user_facing_order() {
        let specs = shortcut_specs();
        assert_eq!(specs.len(), SPEC_ORDER.len());
        for (position, &index) in SPEC_ORDER.iter().enumerate() {
            assert_eq!(
                specs[position],
                spec_of(ACTIONS[index]),
                "shortcut spec at position {position} diverged from the registry"
            );
        }
    }

    #[test]
    fn managed_bindings_mirror_the_registry_terminal_entries() {
        let bindings = managed_bindings();
        assert_eq!(bindings.len(), MANAGED_COUNT);
        let terminals = ACTIONS
            .iter()
            .filter(|entry| matches!(entry.command, ActionCommand::Terminal { .. }))
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), bindings.len());
        for (binding, entry) in bindings.iter().zip(terminals) {
            assert_eq!(
                *binding,
                binding_of(*entry),
                "managed binding {} diverged from the registry",
                entry.name
            );
        }
    }

    #[test]
    fn specs_and_managed_bindings_agree_through_the_registry() {
        for spec in shortcut_specs() {
            let ShortcutCommand::Terminal(action) = spec.command else {
                continue;
            };
            let entry = ACTIONS
                .iter()
                .find(|entry| entry.name == spec.name)
                .unwrap_or_else(|| panic!("registry is missing shortcut {:?}", spec.name));
            let ActionCommand::Terminal {
                action: registered,
                bridge,
            } = entry.command
            else {
                panic!("shortcut {:?} must map to a terminal entry", spec.name);
            };
            assert_eq!(registered, action, "shortcut {:?}", spec.name);
            let binding = binding_for_action(action)
                .unwrap_or_else(|| panic!("no bridge binding for {action:?}"));
            assert_eq!(
                binding.action_id, bridge.action_id,
                "shortcut {:?} bridge id diverged from the registry",
                spec.name
            );
            assert_eq!(
                binding.bridge_chord, bridge.chord,
                "shortcut {:?} bridge chord diverged from the registry",
                spec.name
            );
        }

        for binding in managed_bindings() {
            let matching_specs = shortcut_specs()
                .iter()
                .filter(|spec| {
                    matches!(spec.command, ShortcutCommand::Terminal(action) if action == binding.action)
                })
                .count();
            assert_eq!(
                matching_specs, 1,
                "{} must map to exactly one shortcut spec",
                binding.action_id
            );
        }
    }
}
