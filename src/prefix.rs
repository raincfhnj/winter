use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::model::{Direction, TerminalAction, WindowIdentity};
use crate::registry;

pub const DEFAULT_PREFIX_TIMEOUT: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixConfig {
    pub timeout: Duration,
    pub prefix_chord: KeyChord,
    pub bindings: HashMap<KeyChord, ShortcutCommand>,
}

impl PrefixConfig {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self::with_bindings(
            timeout,
            default_prefix_chord(),
            shortcut_specs()
                .iter()
                .map(|spec| (spec.default_chord, spec.command)),
        )
    }

    #[must_use]
    pub fn with_bindings(
        timeout: Duration,
        prefix_chord: KeyChord,
        bindings: impl IntoIterator<Item = (KeyChord, ShortcutCommand)>,
    ) -> Self {
        Self {
            timeout,
            prefix_chord,
            bindings: bindings.into_iter().collect(),
        }
    }
}

impl Default for PrefixConfig {
    fn default() -> Self {
        Self::new(DEFAULT_PREFIX_TIMEOUT)
    }
}

/// Stable identity for balancing a consumed key-down with repeats and key-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalKey {
    pub scan_code: u32,
    pub extended: bool,
}

impl PhysicalKey {
    #[must_use]
    pub const fn new(scan_code: u32, extended: bool) -> Self {
        Self {
            scan_code,
            extended,
        }
    }
}

/// Logical key identified by US-layout physical key-position semantics.
///
/// Chords are matched against the US keyboard layout: [`LogicalKey::Character`]
/// values refer to the key's position on a US keyboard, not to the legend
/// printed on a non-US keyboard. The platform adapter maps raw virtual-key
/// codes through a fixed US table and never performs an active-layout
/// translation (no `ToUnicodeEx`), so `ctrl+b` always means the physical `B`
/// position regardless of the keyboard layout selected in Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalKey {
    Character(char),
    Arrow(Direction),
    Escape,
    Tab,
    Function(u8),
    Modifier,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub windows: bool,
}

impl Modifiers {
    #[must_use]
    pub const fn new(ctrl: bool, alt: bool, shift: bool, windows: bool) -> Self {
        Self {
            ctrl,
            alt,
            shift,
            windows,
        }
    }

    #[must_use]
    pub const fn has_ctrl_or_alt(self) -> bool {
        self.ctrl || self.alt
    }
}

/// Appends the active modifier names to `parts` in the canonical chord
/// order (`win+ctrl+alt+shift`), omitting inactive modifiers.
///
/// This is the single modifier-prefix builder shared by [`KeyChord`]'s
/// [`Display`](fmt::Display) impl and
/// [`BridgeChord::as_windows_terminal_key`](crate::keymap::BridgeChord::as_windows_terminal_key),
/// so prefix chords and installed bridge keybindings are always spelled
/// from the same modifier sequence. Callers that have no Windows modifier
/// (bridge chords) pass `Modifiers::new(ctrl, alt, shift, false)`, which
/// keeps their historical `ctrl+...` prefix byte-identical.
pub(crate) fn push_modifier_names(parts: &mut Vec<String>, modifiers: Modifiers) {
    if modifiers.windows {
        parts.push("win".to_owned());
    }
    if modifiers.ctrl {
        parts.push("ctrl".to_owned());
    }
    if modifiers.alt {
        parts.push("alt".to_owned());
    }
    if modifiers.shift {
        parts.push("shift".to_owned());
    }
}

/// A user-configurable key chord after platform normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: LogicalKey,
    pub modifiers: Modifiers,
}

impl KeyChord {
    #[must_use]
    pub const fn new(key: LogicalKey, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }

    #[must_use]
    pub fn matches(self, key: LogicalKey, modifiers: Modifiers) -> bool {
        self.key == key && self.modifiers == modifiers
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::with_capacity(5);
        push_modifier_names(&mut parts, self.modifiers);
        parts.push(logical_key_name(self.key));
        formatter.write_str(&parts.join("+"))
    }
}

impl FromStr for KeyChord {
    type Err = KeyChordParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_key_chord(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChordParseError {
    message: String,
}

impl KeyChordParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for KeyChordParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for KeyChordParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutCommand {
    Terminal(TerminalAction),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub name: &'static str,
    pub default_chord: KeyChord,
    pub command: ShortcutCommand,
    pub allow_disabled: bool,
}

pub(crate) const fn chord(key: LogicalKey, ctrl: bool, alt: bool, shift: bool) -> KeyChord {
    KeyChord::new(key, Modifiers::new(ctrl, alt, shift, false))
}

/// Shortcut specs derived from the canonical action registry at compile time.
static SHORTCUT_SPECS: [ShortcutSpec; registry::ACTION_COUNT] = registry::build_shortcut_specs();

#[must_use]
pub const fn default_prefix_chord() -> KeyChord {
    chord(LogicalKey::Character('b'), true, false, false)
}

#[must_use]
pub fn shortcut_specs() -> &'static [ShortcutSpec] {
    &SHORTCUT_SPECS
}

/// Whether a chord is owned by Windows or Windows Terminal and must never be
/// swallowed while the prefix is armed.
///
/// Covers every Windows-key chord, `Alt+Tab`, `Alt+F4`, `Alt+Escape`,
/// `Ctrl+Escape`, and Windows Terminal's own defaults `Ctrl+Tab`,
/// `Ctrl+Shift+Tab`, `Ctrl+Shift+T`, and `Ctrl+Shift+W`. Such chords cancel
/// the armed prefix and pass through, and configuration rejects them as
/// bindings.
#[must_use]
pub const fn is_reserved_system_chord(chord: KeyChord) -> bool {
    if chord.modifiers.windows {
        return true;
    }

    match chord.key {
        LogicalKey::Tab => chord.modifiers.alt || chord.modifiers.ctrl,
        LogicalKey::Escape => chord.modifiers.alt || chord.modifiers.ctrl,
        LogicalKey::Function(4) => chord.modifiers.alt,
        LogicalKey::Character('t' | 'w') => chord.modifiers.ctrl && chord.modifiers.shift,
        _ => false,
    }
}

fn parse_key_chord(value: &str) -> Result<KeyChord, KeyChordParseError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(KeyChordParseError::new("key chord cannot be empty"));
    }

    let mut modifiers = Modifiers::default();
    let mut key = None;
    for raw_part in value.split('+') {
        let part = raw_part.trim().to_ascii_lowercase();
        if part.is_empty() {
            return Err(KeyChordParseError::new(format!(
                "invalid empty token in {value:?}"
            )));
        }
        match part.as_str() {
            "ctrl" | "control" => set_modifier(&mut modifiers.ctrl, "ctrl")?,
            "alt" => set_modifier(&mut modifiers.alt, "alt")?,
            "shift" => set_modifier(&mut modifiers.shift, "shift")?,
            "win" | "windows" | "super" => {
                return Err(KeyChordParseError::new(
                    "the Windows modifier is reserved and cannot be configured",
                ));
            }
            _ => {
                if key.is_some() {
                    return Err(KeyChordParseError::new(format!(
                        "a chord must contain exactly one non-modifier key: {value:?}"
                    )));
                }
                key = Some(parse_logical_key_name(&part)?);
            }
        }
    }

    let key = key
        .ok_or_else(|| KeyChordParseError::new(format!("a chord must include a key: {value:?}")))?;
    Ok(KeyChord::new(key, modifiers))
}

fn set_modifier(target: &mut bool, name: &str) -> Result<(), KeyChordParseError> {
    if *target {
        return Err(KeyChordParseError::new(format!(
            "modifier {name:?} appears more than once"
        )));
    }
    *target = true;
    Ok(())
}

fn parse_logical_key_name(value: &str) -> Result<LogicalKey, KeyChordParseError> {
    crate::keys::parse_key_name(value).ok_or_else(|| {
        KeyChordParseError::new(format!(
            "unsupported key {value:?}; use a-z, 0-9, arrows, F1-F12, or a documented key name"
        ))
    })
}

fn logical_key_name(key: LogicalKey) -> String {
    crate::keys::logical_key_name(key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyTransition {
    Down,
    Up,
}

/// Normalized input consumed by [`PrefixMachine`].
///
/// `foreground_terminal` is `Some` only when the platform layer has verified a
/// supported Windows Terminal foreground window. A non-Terminal foreground is
/// represented by `None` and is never captured while the machine is idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub physical_key: PhysicalKey,
    pub logical_key: LogicalKey,
    pub transition: KeyTransition,
    pub modifiers: Modifiers,
    pub injected: bool,
    pub foreground_terminal: Option<WindowIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDisposition {
    PassThrough,
    Consume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixCommand {
    Dispatch {
        target: WindowIdentity,
        action: TerminalAction,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    Escape,
    UnknownKey,
    Timeout,
    ForegroundChanged,
    SystemShortcut,
    PointerInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixOutcome {
    pub disposition: KeyDisposition,
    pub command: Option<PrefixCommand>,
    pub cancellation: Option<CancelReason>,
}

impl PrefixOutcome {
    const fn pass_through(cancellation: Option<CancelReason>) -> Self {
        Self {
            disposition: KeyDisposition::PassThrough,
            command: None,
            cancellation,
        }
    }

    const fn consume(command: Option<PrefixCommand>, cancellation: Option<CancelReason>) -> Self {
        Self {
            disposition: KeyDisposition::Consume,
            command,
            cancellation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixState {
    Idle,
    Armed {
        target: WindowIdentity,
        deadline: Instant,
        prefix_key: PhysicalKey,
        prefix_key_released: bool,
    },
}

#[derive(Debug, Clone)]
pub struct PrefixMachine {
    config: PrefixConfig,
    state: PrefixState,
    suppressed_keys: HashSet<PhysicalKey>,
}

impl PrefixMachine {
    #[must_use]
    pub fn new(config: PrefixConfig) -> Self {
        Self {
            config,
            state: PrefixState::Idle,
            suppressed_keys: HashSet::new(),
        }
    }

    #[must_use]
    pub const fn config(&self) -> &PrefixConfig {
        &self.config
    }

    #[must_use]
    pub const fn state(&self) -> PrefixState {
        self.state
    }

    #[must_use]
    pub const fn is_armed(&self) -> bool {
        matches!(self.state, PrefixState::Armed { .. })
    }

    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        match self.state {
            PrefixState::Idle => None,
            PrefixState::Armed { deadline, .. } => Some(deadline),
        }
    }

    /// Handles one normalized keyboard event and synchronously decides whether
    /// the low-level hook must pass it on or consume it.
    #[must_use]
    pub fn handle_key_event(&mut self, event: KeyEvent, now: Instant) -> PrefixOutcome {
        let mut cancellation = self.expire(now);

        if self.cancel_if_foreground_changed(event.foreground_terminal) {
            cancellation = Some(CancelReason::ForegroundChanged);
        }

        // Defense-in-depth, not a live production guard: the low-level hook
        // filters injected events before handlers ever run, so `injected` is
        // always false on this path outside unit tests that build events by
        // hand. Should an injected event slip through anyway, it is passed
        // through untouched: never interpreted as user input and never added
        // to the suppression ledger.
        if event.injected {
            return PrefixOutcome::pass_through(cancellation);
        }

        if self.suppressed_keys.contains(&event.physical_key) {
            if event.transition == KeyTransition::Up {
                self.suppressed_keys.remove(&event.physical_key);
                if let PrefixState::Armed {
                    prefix_key,
                    ref mut prefix_key_released,
                    ..
                } = self.state
                {
                    if prefix_key == event.physical_key {
                        *prefix_key_released = true;
                    }
                }
            }

            return PrefixOutcome::consume(None, cancellation);
        }

        if event.transition == KeyTransition::Up {
            return PrefixOutcome::pass_through(cancellation);
        }

        match self.state {
            PrefixState::Idle => self.handle_idle_key_down(event, now, cancellation),
            PrefixState::Armed {
                target,
                prefix_key_released,
                ..
            } => self.handle_armed_key_down(event, target, prefix_key_released, cancellation),
        }
    }

    /// Expires an armed prefix when its deadline is reached, ending the
    /// session and releasing its suppression ledger.
    #[must_use]
    pub fn expire(&mut self, now: Instant) -> Option<CancelReason> {
        if matches!(
            self.state,
            PrefixState::Armed { deadline, .. } if now >= deadline
        ) {
            self.reset_transient_state();
            Some(CancelReason::Timeout)
        } else {
            None
        }
    }

    /// Cancels the prefix on mouse- or system-driven foreground changes.
    #[must_use]
    pub fn observe_foreground(
        &mut self,
        foreground_terminal: Option<WindowIdentity>,
    ) -> Option<CancelReason> {
        self.cancel_if_foreground_changed(foreground_terminal)
            .then_some(CancelReason::ForegroundChanged)
    }

    /// Cancels an armed prefix after an unrelated pointer interaction.
    ///
    /// A due deadline is evaluated first using [`Instant::now`], so an armed
    /// prefix whose timeout has already passed reports [`CancelReason::Timeout`]
    /// instead of `reason`. Ending the session clears the suppression ledger,
    /// so a later key-up for a previously consumed key passes through instead
    /// of being swallowed in other applications.
    #[must_use]
    pub fn cancel(&mut self, reason: CancelReason) -> Option<CancelReason> {
        if let Some(timeout) = self.expire(Instant::now()) {
            return Some(timeout);
        }

        if self.is_armed() {
            self.reset_transient_state();
            Some(reason)
        } else {
            None
        }
    }

    fn handle_idle_key_down(
        &mut self,
        event: KeyEvent,
        now: Instant,
        cancellation: Option<CancelReason>,
    ) -> PrefixOutcome {
        let Some(target) = event.foreground_terminal else {
            return PrefixOutcome::pass_through(cancellation);
        };

        if !self
            .config
            .prefix_chord
            .matches(event.logical_key, event.modifiers)
        {
            return PrefixOutcome::pass_through(cancellation);
        }

        let deadline = now.checked_add(self.config.timeout).unwrap_or(now);
        self.suppressed_keys.insert(event.physical_key);
        self.state = PrefixState::Armed {
            target,
            deadline,
            prefix_key: event.physical_key,
            prefix_key_released: false,
        };

        PrefixOutcome::consume(None, cancellation)
    }

    fn handle_armed_key_down(
        &mut self,
        event: KeyEvent,
        target: WindowIdentity,
        prefix_key_released: bool,
        cancellation: Option<CancelReason>,
    ) -> PrefixOutcome {
        if is_system_shortcut(event.logical_key, event.modifiers) {
            self.state = PrefixState::Idle;
            return PrefixOutcome::pass_through(
                cancellation.or(Some(CancelReason::SystemShortcut)),
            );
        }

        if event.logical_key == LogicalKey::Modifier {
            return PrefixOutcome::pass_through(cancellation);
        }

        if event.logical_key == LogicalKey::Escape {
            self.consume_key(event.physical_key);
            self.state = PrefixState::Idle;
            return PrefixOutcome::consume(None, cancellation.or(Some(CancelReason::Escape)));
        }

        let command = self
            .config
            .bindings
            .get(&KeyChord::new(event.logical_key, event.modifiers))
            .copied();
        if matches!(
            command,
            Some(ShortcutCommand::Terminal(TerminalAction::SendPrefixLiteral))
        ) && !prefix_key_released
        {
            self.consume_key(event.physical_key);
            return PrefixOutcome::consume(None, cancellation);
        }

        let command = command.map(|command| match command {
            ShortcutCommand::Terminal(action) => PrefixCommand::Dispatch { target, action },
            ShortcutCommand::Shutdown => PrefixCommand::Shutdown,
        });

        let Some(command) = command else {
            return self.consume_unknown(event.physical_key, cancellation);
        };

        self.consume_key(event.physical_key);
        self.state = PrefixState::Idle;
        PrefixOutcome::consume(Some(command), cancellation)
    }

    fn consume_unknown(
        &mut self,
        physical_key: PhysicalKey,
        cancellation: Option<CancelReason>,
    ) -> PrefixOutcome {
        self.consume_key(physical_key);
        self.state = PrefixState::Idle;
        PrefixOutcome::consume(None, cancellation.or(Some(CancelReason::UnknownKey)))
    }

    fn consume_key(&mut self, physical_key: PhysicalKey) {
        self.suppressed_keys.insert(physical_key);
    }

    /// Ends the current prefix session and clears its per-session residue so
    /// that a dropped key-up can never leave a key suppressed globally.
    fn reset_transient_state(&mut self) {
        self.state = PrefixState::Idle;
        self.suppressed_keys.clear();
    }

    fn cancel_if_foreground_changed(
        &mut self,
        foreground_terminal: Option<WindowIdentity>,
    ) -> bool {
        let PrefixState::Armed { target, .. } = self.state else {
            return false;
        };

        if foreground_terminal == Some(target) {
            return false;
        }

        self.reset_transient_state();
        true
    }
}

impl Default for PrefixMachine {
    fn default() -> Self {
        Self::new(PrefixConfig::default())
    }
}

#[must_use]
fn is_system_shortcut(key: LogicalKey, modifiers: Modifiers) -> bool {
    is_reserved_system_chord(KeyChord::new(key, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TerminalChannel;

    const PREFIX_KEY: PhysicalKey = PhysicalKey::new(0x30, false);
    const COMMAND_KEY: PhysicalKey = PhysicalKey::new(0x2E, false);

    fn target(hwnd: isize) -> WindowIdentity {
        WindowIdentity {
            hwnd,
            process_id: hwnd as u32 + 100,
            process_started_at_100ns: hwnd as u64 + 1_000,
            channel: TerminalChannel::Stable,
        }
    }

    fn event(
        physical_key: PhysicalKey,
        logical_key: LogicalKey,
        transition: KeyTransition,
        modifiers: Modifiers,
        foreground_terminal: Option<WindowIdentity>,
    ) -> KeyEvent {
        KeyEvent {
            physical_key,
            logical_key,
            transition,
            modifiers,
            injected: false,
            foreground_terminal,
        }
    }

    fn prefix_event(transition: KeyTransition, terminal: Option<WindowIdentity>) -> KeyEvent {
        event(
            PREFIX_KEY,
            LogicalKey::Character('b'),
            transition,
            Modifiers::new(true, false, false, false),
            terminal,
        )
    }

    fn arm(machine: &mut PrefixMachine, now: Instant, terminal: WindowIdentity) {
        let down = machine.handle_key_event(prefix_event(KeyTransition::Down, Some(terminal)), now);
        assert_eq!(down.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());

        let up = machine.handle_key_event(
            prefix_event(KeyTransition::Up, Some(terminal)),
            now + Duration::from_millis(1),
        );
        assert_eq!(up.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());
    }

    #[test]
    fn default_timeout_is_fifteen_hundred_milliseconds() {
        assert_eq!(
            PrefixConfig::default().timeout,
            Duration::from_millis(1_500)
        );
        assert_eq!(PrefixMachine::default().config(), &PrefixConfig::default());
    }

    #[test]
    fn parses_and_formats_documented_key_chords() {
        let cases = [
            "ctrl+b",
            "alt+shift+k",
            "ctrl+left",
            "space",
            "comma",
            "f12",
            "ctrl+left-bracket",
        ];

        for value in cases {
            let chord = value.parse::<KeyChord>().expect("documented chord");
            assert_eq!(chord.to_string(), value);
        }
        assert!("win+k".parse::<KeyChord>().is_err());
        assert!("ctrl+alt".parse::<KeyChord>().is_err());
        assert!("ctrl+a+b".parse::<KeyChord>().is_err());
    }

    /// Every canonical table row round-trips through the public chord
    /// parser and formatter: each accepted name parses to the row's key,
    /// display emits the canonical spelling, and that spelling parses back
    /// to the same key.
    #[test]
    fn canonical_key_rows_round_trip_through_chord_parse_and_display() {
        for row in crate::keys::KEY_DEFS {
            for name in row.names {
                let chord = name
                    .parse::<KeyChord>()
                    .unwrap_or_else(|error| panic!("{name:?} must parse: {error}"));
                assert_eq!(chord.key, row.key, "parse({name:?})");
            }

            let display = KeyChord::new(row.key, Modifiers::default()).to_string();
            assert_eq!(display, row.canonical, "display({:?})", row.key);

            let reparsed = display
                .parse::<KeyChord>()
                .unwrap_or_else(|error| panic!("{display:?} must reparse: {error}"));
            assert_eq!(reparsed.key, row.key, "reparse({display:?})");
        }
    }

    /// Spellings owned by other layers (Windows Terminal chord syntax from
    /// the integration normalizer, runtime-only sentinels, or malformed
    /// forms) must keep failing to parse so configuration acceptance stays
    /// byte-identical.
    #[test]
    fn non_prefix_spellings_stay_rejected() {
        let rejected = [
            "plus", "oemplus", "enter", "return", "pgup", "pgdn", "windows", "super", "modifier",
            "other", "f0", "f13", "f100", "ab", ":", "+", "_", "spacebar",
        ];
        for value in rejected {
            assert!(
                value.parse::<KeyChord>().is_err(),
                "{value:?} must stay rejected"
            );
        }
    }

    #[test]
    fn shortcut_specs_are_unique_and_complete() {
        let names = shortcut_specs()
            .iter()
            .map(|spec| spec.name)
            .collect::<HashSet<_>>();
        let chords = shortcut_specs()
            .iter()
            .map(|spec| spec.default_chord)
            .collect::<HashSet<_>>();

        assert_eq!(shortcut_specs().len(), registry::ACTION_COUNT);
        assert_eq!(names.len(), shortcut_specs().len());
        assert_eq!(chords.len(), shortcut_specs().len());
    }

    #[test]
    fn custom_prefix_and_command_chord_drive_the_machine() {
        let now = Instant::now();
        let terminal = target(20);
        let config = PrefixConfig::with_bindings(
            Duration::from_millis(500),
            "alt+a".parse().expect("valid prefix"),
            [(
                "t".parse().expect("valid command chord"),
                ShortcutCommand::Terminal(TerminalAction::NewTab),
            )],
        );
        let mut machine = PrefixMachine::new(config);

        let prefix_down = machine.handle_key_event(
            event(
                PREFIX_KEY,
                LogicalKey::Character('a'),
                KeyTransition::Down,
                Modifiers::new(false, true, false, false),
                Some(terminal),
            ),
            now,
        );
        assert_eq!(prefix_down.disposition, KeyDisposition::Consume);
        let _ = machine.handle_key_event(
            event(
                PREFIX_KEY,
                LogicalKey::Character('a'),
                KeyTransition::Up,
                Modifiers::new(false, true, false, false),
                Some(terminal),
            ),
            now + Duration::from_millis(1),
        );
        let command = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('t'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );

        assert_eq!(
            command.command,
            Some(PrefixCommand::Dispatch {
                target: terminal,
                action: TerminalAction::NewTab,
            })
        );
    }

    #[test]
    fn prefix_only_arms_in_a_verified_terminal() {
        let now = Instant::now();
        let mut machine = PrefixMachine::default();

        let outside = machine.handle_key_event(prefix_event(KeyTransition::Down, None), now);
        assert_eq!(outside.disposition, KeyDisposition::PassThrough);
        assert!(!machine.is_armed());

        let inside =
            machine.handle_key_event(prefix_event(KeyTransition::Down, Some(target(1))), now);
        assert_eq!(inside.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());
        assert_eq!(machine.deadline(), now.checked_add(DEFAULT_PREFIX_TIMEOUT));
    }

    #[test]
    fn injected_prefix_passes_through() {
        let mut machine = PrefixMachine::default();
        let mut injected = prefix_event(KeyTransition::Down, Some(target(1)));
        injected.injected = true;

        let outcome = machine.handle_key_event(injected, Instant::now());

        assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
        assert!(!machine.is_armed());
    }

    #[test]
    fn arrows_map_to_focus_split_and_resize() {
        let now = Instant::now();
        let terminal = target(1);
        let cases = [
            (
                Modifiers::default(),
                TerminalAction::FocusPane {
                    direction: Direction::Left,
                },
            ),
            (
                Modifiers::new(false, false, true, false),
                TerminalAction::SplitPane {
                    direction: Direction::Left,
                },
            ),
            (
                Modifiers::new(true, false, false, false),
                TerminalAction::ResizePane {
                    direction: Direction::Left,
                },
            ),
        ];

        for (modifiers, expected_action) in cases {
            let mut machine = PrefixMachine::default();
            arm(&mut machine, now, terminal);
            let outcome = machine.handle_key_event(
                event(
                    COMMAND_KEY,
                    LogicalKey::Arrow(Direction::Left),
                    KeyTransition::Down,
                    modifiers,
                    Some(terminal),
                ),
                now + Duration::from_millis(2),
            );

            assert_eq!(
                outcome.command,
                Some(PrefixCommand::Dispatch {
                    target: terminal,
                    action: expected_action,
                })
            );
            assert!(!machine.is_armed());
        }
    }

    #[test]
    fn character_commands_cover_tabs_panes_zoom_rename_and_shutdown() {
        let now = Instant::now();
        let terminal = target(2);
        let cases = [
            ('c', Some(TerminalAction::NewTab)),
            ('n', Some(TerminalAction::NextTab)),
            ('p', Some(TerminalAction::PreviousTab)),
            ('7', Some(TerminalAction::ActivateTab { index: 7 })),
            ('x', Some(TerminalAction::ClosePane)),
            ('z', Some(TerminalAction::TogglePaneZoom)),
            (',', Some(TerminalAction::RenameTab)),
            ('q', None),
        ];

        for (character, action) in cases {
            let mut machine = PrefixMachine::default();
            arm(&mut machine, now, terminal);
            let outcome = machine.handle_key_event(
                event(
                    COMMAND_KEY,
                    LogicalKey::Character(character),
                    KeyTransition::Down,
                    Modifiers::default(),
                    Some(terminal),
                ),
                now + Duration::from_millis(2),
            );

            let expected = action.map_or(Some(PrefixCommand::Shutdown), |action| {
                Some(PrefixCommand::Dispatch {
                    target: terminal,
                    action,
                })
            });
            assert_eq!(outcome.command, expected);
            assert_eq!(outcome.disposition, KeyDisposition::Consume);
        }
    }

    #[test]
    fn consumed_key_repeats_and_key_up_are_also_consumed() {
        let now = Instant::now();
        let terminal = target(3);
        let mut machine = PrefixMachine::default();
        arm(&mut machine, now, terminal);
        let command = event(
            COMMAND_KEY,
            LogicalKey::Character('c'),
            KeyTransition::Down,
            Modifiers::default(),
            Some(terminal),
        );

        assert!(
            machine
                .handle_key_event(command, now + Duration::from_millis(2))
                .command
                .is_some()
        );
        let repeat = machine.handle_key_event(command, now + Duration::from_millis(3));
        assert_eq!(repeat.disposition, KeyDisposition::Consume);
        assert_eq!(repeat.command, None);

        let mut key_up = command;
        key_up.transition = KeyTransition::Up;
        assert_eq!(
            machine
                .handle_key_event(key_up, now + Duration::from_millis(4))
                .disposition,
            KeyDisposition::Consume
        );
        assert_eq!(
            machine
                .handle_key_event(command, now + Duration::from_millis(5))
                .disposition,
            KeyDisposition::PassThrough
        );
    }

    #[test]
    fn prefix_repeat_does_not_extend_the_deadline_or_send_literal() {
        let now = Instant::now();
        let terminal = target(4);
        let mut machine = PrefixMachine::default();
        let prefix = prefix_event(KeyTransition::Down, Some(terminal));

        let _ = machine.handle_key_event(prefix, now);
        let deadline = machine.deadline();
        let repeat = machine.handle_key_event(prefix, now + Duration::from_millis(100));

        assert_eq!(repeat.disposition, KeyDisposition::Consume);
        assert_eq!(repeat.command, None);
        assert_eq!(machine.deadline(), deadline);
    }

    #[test]
    fn released_prefix_then_b_sends_literal() {
        let now = Instant::now();
        let terminal = target(5);
        let mut machine = PrefixMachine::default();
        arm(&mut machine, now, terminal);

        let outcome = machine.handle_key_event(
            event(
                PREFIX_KEY,
                LogicalKey::Character('b'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );

        assert_eq!(
            outcome.command,
            Some(PrefixCommand::Dispatch {
                target: terminal,
                action: TerminalAction::SendPrefixLiteral,
            })
        );
    }

    #[test]
    fn escape_and_unknown_keys_are_consumed_and_cancel() {
        let now = Instant::now();
        let terminal = target(6);

        let mut escape_machine = PrefixMachine::default();
        arm(&mut escape_machine, now, terminal);
        let escape = escape_machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Escape,
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );
        assert_eq!(escape.disposition, KeyDisposition::Consume);
        assert_eq!(escape.cancellation, Some(CancelReason::Escape));

        let mut unknown_machine = PrefixMachine::default();
        arm(&mut unknown_machine, now, terminal);
        let unknown = unknown_machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('v'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );
        assert_eq!(unknown.disposition, KeyDisposition::Consume);
        assert_eq!(unknown.cancellation, Some(CancelReason::UnknownKey));
    }

    #[test]
    fn timeout_wins_at_the_deadline_and_does_not_replay_prefix() {
        let now = Instant::now();
        let terminal = target(7);
        let mut machine = PrefixMachine::new(PrefixConfig::new(Duration::from_millis(10)));
        arm(&mut machine, now, terminal);

        let outcome = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('c'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(10),
        );

        assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
        assert_eq!(outcome.command, None);
        assert_eq!(outcome.cancellation, Some(CancelReason::Timeout));
    }

    #[test]
    fn foreground_change_cancels_and_passes_the_new_apps_key() {
        let now = Instant::now();
        let original = target(8);
        let mut machine = PrefixMachine::default();
        arm(&mut machine, now, original);

        let outcome = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('c'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(target(9)),
            ),
            now + Duration::from_millis(2),
        );

        assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
        assert_eq!(outcome.cancellation, Some(CancelReason::ForegroundChanged));
        assert!(!machine.is_armed());
    }

    #[test]
    fn pointer_input_cancels_and_releases_the_suppressed_prefix_key() {
        let now = Instant::now();
        let terminal = target(81);
        let mut machine = PrefixMachine::new(PrefixConfig::new(Duration::from_secs(60)));
        let _ = machine.handle_key_event(prefix_event(KeyTransition::Down, Some(terminal)), now);

        assert_eq!(
            machine.cancel(CancelReason::PointerInput),
            Some(CancelReason::PointerInput)
        );
        assert!(!machine.is_armed());
        let key_up = machine.handle_key_event(
            prefix_event(KeyTransition::Up, Some(terminal)),
            now + Duration::from_millis(1),
        );
        assert_eq!(key_up.disposition, KeyDisposition::PassThrough);
        assert_eq!(machine.cancel(CancelReason::PointerInput), None);
    }

    #[test]
    fn foreground_change_releases_the_suppressed_prefix_key() {
        let now = Instant::now();
        let terminal = target(10);
        let mut machine = PrefixMachine::default();
        let _ = machine.handle_key_event(prefix_event(KeyTransition::Down, Some(terminal)), now);

        let outcome = machine.handle_key_event(
            prefix_event(KeyTransition::Up, None),
            now + Duration::from_millis(1),
        );

        assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
        assert_eq!(outcome.cancellation, Some(CancelReason::ForegroundChanged));
    }

    #[test]
    fn system_shortcuts_cancel_but_pass_through() {
        let now = Instant::now();
        let terminal = target(11);
        let cases = [
            (LogicalKey::Tab, Modifiers::new(false, true, false, false)),
            (
                LogicalKey::Escape,
                Modifiers::new(true, false, false, false),
            ),
            (
                LogicalKey::Function(4),
                Modifiers::new(false, true, false, false),
            ),
            (
                LogicalKey::Character('r'),
                Modifiers::new(false, false, false, true),
            ),
        ];

        for (logical_key, modifiers) in cases {
            let mut machine = PrefixMachine::default();
            arm(&mut machine, now, terminal);
            let outcome = machine.handle_key_event(
                event(
                    COMMAND_KEY,
                    logical_key,
                    KeyTransition::Down,
                    modifiers,
                    Some(terminal),
                ),
                now + Duration::from_millis(2),
            );

            assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
            assert_eq!(outcome.cancellation, Some(CancelReason::SystemShortcut));
        }
    }

    #[test]
    fn modifier_only_and_injected_events_do_not_steal_the_prefix() {
        let now = Instant::now();
        let terminal = target(12);
        let mut machine = PrefixMachine::default();
        arm(&mut machine, now, terminal);

        let modifier = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Modifier,
                KeyTransition::Down,
                Modifiers::new(false, false, true, false),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );
        assert_eq!(modifier.disposition, KeyDisposition::PassThrough);
        assert!(machine.is_armed());

        let mut injected = event(
            COMMAND_KEY,
            LogicalKey::Character('c'),
            KeyTransition::Down,
            Modifiers::default(),
            Some(terminal),
        );
        injected.injected = true;
        let outcome = machine.handle_key_event(injected, now + Duration::from_millis(3));
        assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
        assert!(machine.is_armed());
    }

    #[test]
    fn foreground_observer_cancels_mouse_driven_window_changes() {
        let now = Instant::now();
        let terminal = target(13);
        let mut machine = PrefixMachine::default();
        arm(&mut machine, now, terminal);

        assert_eq!(
            machine.observe_foreground(None),
            Some(CancelReason::ForegroundChanged)
        );
        assert!(!machine.is_armed());
    }

    #[test]
    fn expired_session_releases_the_suppressed_prefix_key() {
        let now = Instant::now();
        let terminal = target(30);
        let mut machine = PrefixMachine::new(PrefixConfig::new(Duration::from_millis(10)));

        let down = machine.handle_key_event(prefix_event(KeyTransition::Down, Some(terminal)), now);
        assert_eq!(down.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());
        assert!(machine.suppressed_keys.contains(&PREFIX_KEY));

        let later = now + Duration::from_millis(10);
        let other_app_key = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('v'),
                KeyTransition::Down,
                Modifiers::default(),
                None,
            ),
            later,
        );
        assert_eq!(other_app_key.disposition, KeyDisposition::PassThrough);
        assert_eq!(other_app_key.cancellation, Some(CancelReason::Timeout));
        assert!(!machine.is_armed());
        assert!(machine.suppressed_keys.is_empty());

        let next_press = machine.handle_key_event(
            prefix_event(KeyTransition::Down, None),
            later + Duration::from_millis(1),
        );
        assert_eq!(next_press.disposition, KeyDisposition::PassThrough);
    }

    #[test]
    fn pointer_cancel_past_the_deadline_reports_timeout() {
        let now = Instant::now();
        let terminal = target(31);
        let mut machine = PrefixMachine::new(PrefixConfig::new(Duration::ZERO));

        let down = machine.handle_key_event(prefix_event(KeyTransition::Down, Some(terminal)), now);
        assert_eq!(down.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());

        assert_eq!(
            machine.cancel(CancelReason::PointerInput),
            Some(CancelReason::Timeout)
        );
        assert!(!machine.is_armed());
        assert!(machine.suppressed_keys.is_empty());
        assert_eq!(machine.cancel(CancelReason::PointerInput), None);
    }

    #[test]
    fn display_includes_the_windows_modifier() {
        let with_windows = KeyChord::new(
            LogicalKey::Character('r'),
            Modifiers::new(true, false, true, true),
        );
        let without_windows = KeyChord::new(
            LogicalKey::Character('r'),
            Modifiers::new(true, false, true, false),
        );

        assert_eq!(with_windows.to_string(), "win+ctrl+shift+r");
        assert_ne!(with_windows.to_string(), without_windows.to_string());
    }

    #[test]
    fn windows_terminal_default_chords_pass_through_while_armed() {
        let now = Instant::now();
        let terminal = target(33);
        let cases = [
            (LogicalKey::Tab, Modifiers::new(true, false, false, false)),
            (LogicalKey::Tab, Modifiers::new(true, false, true, false)),
            (
                LogicalKey::Character('t'),
                Modifiers::new(true, false, true, false),
            ),
            (
                LogicalKey::Character('w'),
                Modifiers::new(true, false, true, false),
            ),
        ];

        for (logical_key, modifiers) in cases {
            let mut machine = PrefixMachine::default();
            arm(&mut machine, now, terminal);
            let outcome = machine.handle_key_event(
                event(
                    COMMAND_KEY,
                    logical_key,
                    KeyTransition::Down,
                    modifiers,
                    Some(terminal),
                ),
                now + Duration::from_millis(2),
            );

            assert_eq!(outcome.disposition, KeyDisposition::PassThrough);
            assert_eq!(outcome.cancellation, Some(CancelReason::SystemShortcut));
            assert!(!machine.is_armed());
        }

        assert!(!is_reserved_system_chord(KeyChord::new(
            LogicalKey::Character('t'),
            Modifiers::new(true, false, false, false)
        )));
        assert!(!is_reserved_system_chord(KeyChord::new(
            LogicalKey::Character('w'),
            Modifiers::new(true, false, false, false)
        )));
    }

    #[test]
    fn armed_key_down_forwards_a_precomputed_cancellation() {
        let terminal = target(34);
        let mut machine = PrefixMachine::default();

        let unknown = machine.handle_armed_key_down(
            event(
                COMMAND_KEY,
                LogicalKey::Character('v'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            terminal,
            true,
            Some(CancelReason::Timeout),
        );
        assert_eq!(unknown.disposition, KeyDisposition::Consume);
        assert_eq!(unknown.cancellation, Some(CancelReason::Timeout));

        let dispatch = machine.handle_armed_key_down(
            event(
                COMMAND_KEY,
                LogicalKey::Character('c'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            terminal,
            true,
            Some(CancelReason::Timeout),
        );
        assert_eq!(
            dispatch.command,
            Some(PrefixCommand::Dispatch {
                target: terminal,
                action: TerminalAction::NewTab,
            })
        );
        assert_eq!(dispatch.cancellation, Some(CancelReason::Timeout));
    }

    #[test]
    fn gated_send_prefix_literal_is_silently_consumed_while_prefix_is_held() {
        let now = Instant::now();
        let terminal = target(35);
        let config = PrefixConfig::with_bindings(
            Duration::from_millis(500),
            "alt+a".parse().expect("valid prefix"),
            [(
                "b".parse().expect("valid command chord"),
                ShortcutCommand::Terminal(TerminalAction::SendPrefixLiteral),
            )],
        );
        let mut machine = PrefixMachine::new(config);

        let prefix_down = machine.handle_key_event(
            event(
                PREFIX_KEY,
                LogicalKey::Character('a'),
                KeyTransition::Down,
                Modifiers::new(false, true, false, false),
                Some(terminal),
            ),
            now,
        );
        assert_eq!(prefix_down.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());

        let gated = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('b'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(1),
        );
        assert_eq!(gated.disposition, KeyDisposition::Consume);
        assert_eq!(gated.command, None);
        assert_eq!(gated.cancellation, None);
        assert!(machine.is_armed());

        let gated_up = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('b'),
                KeyTransition::Up,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(2),
        );
        assert_eq!(gated_up.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());

        let prefix_up = machine.handle_key_event(
            event(
                PREFIX_KEY,
                LogicalKey::Character('a'),
                KeyTransition::Up,
                Modifiers::new(false, true, false, false),
                Some(terminal),
            ),
            now + Duration::from_millis(3),
        );
        assert_eq!(prefix_up.disposition, KeyDisposition::Consume);
        assert!(machine.is_armed());

        let literal = machine.handle_key_event(
            event(
                COMMAND_KEY,
                LogicalKey::Character('b'),
                KeyTransition::Down,
                Modifiers::default(),
                Some(terminal),
            ),
            now + Duration::from_millis(4),
        );
        assert_eq!(
            literal.command,
            Some(PrefixCommand::Dispatch {
                target: terminal,
                action: TerminalAction::SendPrefixLiteral,
            })
        );
        assert_eq!(literal.disposition, KeyDisposition::Consume);
        assert!(!machine.is_armed());
    }
}
