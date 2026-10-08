use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::TerminalAction;
use crate::prefix::{Modifiers, push_modifier_names};
use crate::registry;

const ACTION_ID_PREFIX: &str = "User.Winter.";

/// A synthetic function-key chord managed by Winter.
///
/// Managed chords deliberately use synthetic high function keys so they do not
/// overlap with the product's user-facing prefix bindings. F16 and F17 are
/// excluded because live Stable 1.24 validation did not dispatch them reliably.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub function_key: u8,
}

impl BridgeChord {
    #[must_use]
    pub const fn new(ctrl: bool, alt: bool, shift: bool, function_key: u8) -> Self {
        Self {
            ctrl,
            alt,
            shift,
            function_key,
        }
    }

    /// Returns the Windows Terminal `keys` spelling of this chord, e.g.
    /// `ctrl+alt+shift+f13`. Bridge chords never carry the Windows modifier,
    /// so the shared modifier builder runs with `windows` cleared.
    #[must_use]
    pub fn as_windows_terminal_key(self) -> String {
        let mut parts = Vec::with_capacity(4);
        push_modifier_names(
            &mut parts,
            Modifiers::new(self.ctrl, self.alt, self.shift, false),
        );
        parts.push(format!("f{}", self.function_key));
        parts.join("+")
    }
}

impl fmt::Display for BridgeChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_windows_terminal_key())
    }
}

/// One Windows Terminal action and its private synthetic key binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ManagedBinding {
    pub action: TerminalAction,
    pub action_id: &'static str,
    pub bridge_chord: BridgeChord,
}

impl ManagedBinding {
    /// Returns the value for an entry's `command` property in Terminal settings.
    #[must_use]
    pub fn terminal_command_json(self) -> Value {
        match self.action {
            TerminalAction::SendPrefixLiteral => {
                json!({ "action": "sendInput", "input": "\u{0002}" })
            }
            TerminalAction::SplitPane { direction } => json!({
                "action": "splitPane",
                "split": direction.as_str(),
                "splitMode": "duplicate",
            }),
            TerminalAction::FocusPane { direction } => json!({
                "action": "moveFocus",
                "direction": direction.as_str(),
            }),
            TerminalAction::ResizePane { direction } => json!({
                "action": "resizePane",
                "direction": direction.as_str(),
            }),
            TerminalAction::NewTab => json!({ "action": "newTab" }),
            TerminalAction::NextTab => json!({ "action": "nextTab" }),
            TerminalAction::PreviousTab => json!({ "action": "prevTab" }),
            TerminalAction::ActivateTab { index } => {
                json!({ "action": "switchToTab", "index": index })
            }
            TerminalAction::ClosePane => json!({ "action": "closePane" }),
            TerminalAction::TogglePaneZoom => json!({ "action": "togglePaneZoom" }),
            TerminalAction::RenameTab => json!({ "action": "openTabRenamer" }),
        }
    }

    /// Returns an entry suitable for Windows Terminal's `actions` array.
    #[must_use]
    pub fn action_definition_json(self) -> Value {
        json!({
            "id": self.action_id,
            "command": self.terminal_command_json(),
        })
    }

    /// Returns an entry suitable for Windows Terminal's `keybindings` array.
    #[must_use]
    pub fn keybinding_definition_json(self) -> Value {
        json!({
            "id": self.action_id,
            "keys": self.bridge_chord.as_windows_terminal_key(),
        })
    }
}

/// Managed bindings derived from the canonical action registry at compile
/// time. The registry order is preserved verbatim so installed fragments and
/// keybindings stay byte-identical across releases.
static MANAGED_BINDINGS: [ManagedBinding; registry::MANAGED_COUNT] =
    registry::build_managed_bindings();

#[must_use]
pub fn managed_bindings() -> &'static [ManagedBinding] {
    &MANAGED_BINDINGS
}

/// Returns the managed bridge binding registered for `action`, or [`None`]
/// when the registry has no bridge entry for it.
///
/// Load-bearing for install/doctor parity, not only for dispatch: the action
/// worker resolves bridge chords through this table, and
/// [`bridge_is_ready`](crate::bridge_is_ready) requires every target's
/// installed `(id, chord)` count to equal the length of
/// [`managed_bindings`]. Every registry row must therefore keep its binding
/// even when the controller never dispatches the action through the bridge —
/// most notably [`TerminalAction::SendPrefixLiteral`], which
/// `HookDispatcher::on_keyboard` intercepts and rewrites into a literal
/// prefix injection before the worker queue, so its bridge chord is inert
/// for dispatch yet still installed. Dropping that row would leave existing
/// installs with more installed bindings than this table expects and fail
/// the doctor readiness check until reinstall.
#[must_use]
pub fn binding_for_action(action: TerminalAction) -> Option<&'static ManagedBinding> {
    MANAGED_BINDINGS
        .iter()
        .find(|binding| binding.action == action)
}

#[must_use]
pub const fn action_id_prefix() -> &'static str {
    ACTION_ID_PREFIX
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::model::Direction;

    #[test]
    fn managed_table_is_complete_and_unique() {
        assert_eq!(managed_bindings().len(), registry::MANAGED_COUNT);

        let actions = managed_bindings()
            .iter()
            .map(|binding| binding.action)
            .collect::<HashSet<_>>();
        let ids = managed_bindings()
            .iter()
            .map(|binding| binding.action_id)
            .collect::<HashSet<_>>();
        let chords = managed_bindings()
            .iter()
            .map(|binding| binding.bridge_chord)
            .collect::<HashSet<_>>();

        assert_eq!(actions.len(), registry::MANAGED_COUNT);
        assert_eq!(ids.len(), registry::MANAGED_COUNT);
        assert_eq!(chords.len(), registry::MANAGED_COUNT);
        assert!(
            managed_bindings()
                .iter()
                .all(|binding| binding.action_id.starts_with(action_id_prefix()))
        );
    }

    #[test]
    fn bridge_chords_use_only_the_live_verified_function_key_subset() {
        let reliable_function_keys = [13, 14, 15, 18, 19, 20, 21, 22, 23, 24];
        assert!(managed_bindings().iter().all(|binding| {
            reliable_function_keys.contains(&binding.bridge_chord.function_key)
        }));
    }

    #[test]
    fn bridge_chords_use_only_reserved_modifier_groups() {
        assert!(managed_bindings().iter().all(|binding| {
            matches!(
                (
                    binding.bridge_chord.ctrl,
                    binding.bridge_chord.alt,
                    binding.bridge_chord.shift,
                ),
                (true, true, true) | (true, false, true) | (false, true, true)
            )
        }));
    }

    #[test]
    fn every_binding_round_trips_by_action() {
        for binding in managed_bindings() {
            assert_eq!(binding_for_action(binding.action), Some(binding));
        }
    }

    #[test]
    fn bridge_chord_uses_windows_terminal_syntax() {
        assert_eq!(
            BridgeChord::new(true, true, true, 13).as_windows_terminal_key(),
            "ctrl+alt+shift+f13"
        );
        assert_eq!(
            BridgeChord::new(false, true, true, 17).to_string(),
            "alt+shift+f17"
        );
    }

    #[test]
    fn command_json_covers_parameterized_actions() {
        let split = binding_for_action(TerminalAction::SplitPane {
            direction: Direction::Right,
        })
        .expect("split-right binding");
        assert_eq!(
            split.terminal_command_json(),
            json!({
                "action": "splitPane",
                "split": "right",
                "splitMode": "duplicate",
            })
        );

        let tab = binding_for_action(TerminalAction::ActivateTab { index: 9 })
            .expect("activate-tab-9 binding");
        assert_eq!(
            tab.terminal_command_json(),
            json!({ "action": "switchToTab", "index": 9 })
        );
    }

    #[test]
    fn fragment_entries_reference_the_same_action_id() {
        let binding =
            binding_for_action(TerminalAction::SendPrefixLiteral).expect("literal-prefix binding");

        assert_eq!(
            binding.action_definition_json(),
            json!({
                "id": "User.Winter.SendPrefixLiteral",
                "command": { "action": "sendInput", "input": "\u{0002}" },
            })
        );
        assert_eq!(
            binding.keybinding_definition_json(),
            json!({
                "id": "User.Winter.SendPrefixLiteral",
                "keys": "ctrl+shift+f15",
            })
        );
    }
}
