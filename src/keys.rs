//! Canonical key table shared by configuration parsing, display, and both
//! virtual-key maps.
//!
//! [`KEY_DEFS`] is the single source of truth for the three key-code views
//! that previously had to stay consistent only by coincidence (review
//! finding D3):
//!
//! - configuration name parsing and display round-trips in [`crate::prefix`],
//! - the forward virtual-key → [`LogicalKey`] map consumed by the keyboard
//!   normalizer in [`crate::controller`],
//! - the reverse [`LogicalKey`] → virtual-key map used to inject the literal
//!   Prefix chord.
//!
//! Every row carries its accepted names, its canonical display spelling, the
//! logical key, and (when injectable) the US-layout virtual key that maps
//! both ways. The const assertions at the bottom reject duplicate names,
//! aliases, virtual keys, or logical keys, pin every row's virtual key to the
//! expected Windows code, and keep the historical single-character spelling
//! set in step with the table while the crate compiles.

use crate::model::Direction;
use crate::prefix::LogicalKey;

/// Punctuation spellings accepted as a single-character key name.
///
/// Letters and digits are accepted through `is_ascii_alphanumeric`; this set
/// covers the remaining punctuation. Each character here must have a
/// canonical row so the single-character fallthrough and [`KEY_DEFS`] cannot
/// drift apart, and every ASCII punctuation row must appear here in return —
/// both directions are asserted at the bottom of this module. The space row
/// is the one intentionally unspellable character: `space` is its only name.
const SINGLE_CHAR_PUNCT: &str = ",.;/\\-='[]`";

// US-layout virtual keys for the canonical table. These stay `u32` to match
// the raw virtual-key type the low-level hook reports; rows store the `u16`
// `VIRTUAL_KEY` payload.
pub const VK_TAB: u32 = 0x09;
pub const VK_ESCAPE: u32 = 0x1b;
pub const VK_SPACE: u32 = 0x20;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_F1: u32 = 0x70;
pub const VK_F2: u32 = 0x71;
pub const VK_F3: u32 = 0x72;
pub const VK_F4: u32 = 0x73;
pub const VK_F5: u32 = 0x74;
pub const VK_F6: u32 = 0x75;
pub const VK_F7: u32 = 0x76;
pub const VK_F8: u32 = 0x77;
pub const VK_F9: u32 = 0x78;
pub const VK_F10: u32 = 0x79;
pub const VK_F11: u32 = 0x7a;
pub const VK_F12: u32 = 0x7b;
pub const VK_OEM_SEMICOLON: u32 = 0xba;
pub const VK_OEM_EQUALS: u32 = 0xbb;
pub const VK_OEM_COMMA: u32 = 0xbc;
pub const VK_OEM_MINUS: u32 = 0xbd;
pub const VK_OEM_PERIOD: u32 = 0xbe;
pub const VK_OEM_SLASH: u32 = 0xbf;
pub const VK_OEM_BACKTICK: u32 = 0xc0;
pub const VK_OEM_LEFT_BRACKET: u32 = 0xdb;
pub const VK_OEM_BACKSLASH: u32 = 0xdc;
pub const VK_OEM_RIGHT_BRACKET: u32 = 0xdd;
pub const VK_OEM_QUOTE: u32 = 0xde;

/// One canonical key row: every accepted configuration name, the canonical
/// display spelling, the logical key it denotes, and (when injectable) the
/// US-layout virtual key that maps both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDef {
    /// Accepted configuration names, all lowercase; always includes
    /// [`Self::canonical`].
    pub names: &'static [&'static str],
    /// Canonical spelling emitted by display, guaranteed to parse back to
    /// [`Self::key`].
    pub canonical: &'static str,
    /// The logical key this row describes.
    pub key: LogicalKey,
    /// The US-layout virtual key for this key.
    ///
    /// `None` marks a recognized-but-not-injectable key: the forward map
    /// still recognizes it (see [`logical_key_for_vk`]) but the reverse map
    /// refuses it. The only such row today is Escape, which cancels the
    /// armed prefix and which configuration rejects as a prefix or shortcut
    /// chord, so no injectable chord can ever reference it.
    pub vk: Option<u16>,
}

const fn key_def(
    names: &'static [&'static str],
    canonical: &'static str,
    key: LogicalKey,
    vk: u32,
) -> KeyDef {
    // Every canonical table VK fits in a single byte, so the widening
    // downcast to the VIRTUAL_KEY payload cannot lose information.
    KeyDef {
        names,
        canonical,
        key,
        vk: Some(vk as u16),
    }
}

/// Every configurable key in canonical order.
pub const KEY_DEFS: &[KeyDef] = &[
    key_def(
        &["left"],
        "left",
        LogicalKey::Arrow(Direction::Left),
        VK_LEFT,
    ),
    key_def(
        &["right"],
        "right",
        LogicalKey::Arrow(Direction::Right),
        VK_RIGHT,
    ),
    key_def(&["up"], "up", LogicalKey::Arrow(Direction::Up), VK_UP),
    key_def(
        &["down"],
        "down",
        LogicalKey::Arrow(Direction::Down),
        VK_DOWN,
    ),
    // Escape is recognized for prefix cancellation but is never injectable:
    // reverse lookup finds this row and returns its `None` virtual key, and
    // configuration rejects Escape chords outright. Exactly one such row may
    // exist (asserted below).
    KeyDef {
        names: &["escape", "esc"],
        canonical: "escape",
        key: LogicalKey::Escape,
        vk: None,
    },
    key_def(&["tab"], "tab", LogicalKey::Tab, VK_TAB),
    key_def(&["space"], "space", LogicalKey::Character(' '), VK_SPACE),
    key_def(&["f1"], "f1", LogicalKey::Function(1), VK_F1),
    key_def(&["f2"], "f2", LogicalKey::Function(2), VK_F2),
    key_def(&["f3"], "f3", LogicalKey::Function(3), VK_F3),
    key_def(&["f4"], "f4", LogicalKey::Function(4), VK_F4),
    key_def(&["f5"], "f5", LogicalKey::Function(5), VK_F5),
    key_def(&["f6"], "f6", LogicalKey::Function(6), VK_F6),
    key_def(&["f7"], "f7", LogicalKey::Function(7), VK_F7),
    key_def(&["f8"], "f8", LogicalKey::Function(8), VK_F8),
    key_def(&["f9"], "f9", LogicalKey::Function(9), VK_F9),
    key_def(&["f10"], "f10", LogicalKey::Function(10), VK_F10),
    key_def(&["f11"], "f11", LogicalKey::Function(11), VK_F11),
    key_def(&["f12"], "f12", LogicalKey::Function(12), VK_F12),
    key_def(
        &["comma"],
        "comma",
        LogicalKey::Character(','),
        VK_OEM_COMMA,
    ),
    key_def(
        &["period", "dot"],
        "period",
        LogicalKey::Character('.'),
        VK_OEM_PERIOD,
    ),
    key_def(
        &["semicolon"],
        "semicolon",
        LogicalKey::Character(';'),
        VK_OEM_SEMICOLON,
    ),
    key_def(
        &["slash"],
        "slash",
        LogicalKey::Character('/'),
        VK_OEM_SLASH,
    ),
    key_def(
        &["backslash"],
        "backslash",
        LogicalKey::Character('\\'),
        VK_OEM_BACKSLASH,
    ),
    key_def(
        &["minus"],
        "minus",
        LogicalKey::Character('-'),
        VK_OEM_MINUS,
    ),
    key_def(
        &["equals"],
        "equals",
        LogicalKey::Character('='),
        VK_OEM_EQUALS,
    ),
    key_def(
        &["quote"],
        "quote",
        LogicalKey::Character('\''),
        VK_OEM_QUOTE,
    ),
    key_def(
        &["backtick"],
        "backtick",
        LogicalKey::Character('`'),
        VK_OEM_BACKTICK,
    ),
    key_def(
        &["left-bracket", "left_bracket"],
        "left-bracket",
        LogicalKey::Character('['),
        VK_OEM_LEFT_BRACKET,
    ),
    key_def(
        &["right-bracket", "right_bracket"],
        "right-bracket",
        LogicalKey::Character(']'),
        VK_OEM_RIGHT_BRACKET,
    ),
    // Digits: VK_0..VK_9 are the ASCII digit codes.
    key_def(&["0"], "0", LogicalKey::Character('0'), b'0' as u32),
    key_def(&["1"], "1", LogicalKey::Character('1'), b'1' as u32),
    key_def(&["2"], "2", LogicalKey::Character('2'), b'2' as u32),
    key_def(&["3"], "3", LogicalKey::Character('3'), b'3' as u32),
    key_def(&["4"], "4", LogicalKey::Character('4'), b'4' as u32),
    key_def(&["5"], "5", LogicalKey::Character('5'), b'5' as u32),
    key_def(&["6"], "6", LogicalKey::Character('6'), b'6' as u32),
    key_def(&["7"], "7", LogicalKey::Character('7'), b'7' as u32),
    key_def(&["8"], "8", LogicalKey::Character('8'), b'8' as u32),
    key_def(&["9"], "9", LogicalKey::Character('9'), b'9' as u32),
    // Letters: VK_A..VK_Z are the uppercase ASCII codes; the forward map
    // normalizes them to lowercase characters.
    key_def(&["a"], "a", LogicalKey::Character('a'), b'A' as u32),
    key_def(&["b"], "b", LogicalKey::Character('b'), b'B' as u32),
    key_def(&["c"], "c", LogicalKey::Character('c'), b'C' as u32),
    key_def(&["d"], "d", LogicalKey::Character('d'), b'D' as u32),
    key_def(&["e"], "e", LogicalKey::Character('e'), b'E' as u32),
    key_def(&["f"], "f", LogicalKey::Character('f'), b'F' as u32),
    key_def(&["g"], "g", LogicalKey::Character('g'), b'G' as u32),
    key_def(&["h"], "h", LogicalKey::Character('h'), b'H' as u32),
    key_def(&["i"], "i", LogicalKey::Character('i'), b'I' as u32),
    key_def(&["j"], "j", LogicalKey::Character('j'), b'J' as u32),
    key_def(&["k"], "k", LogicalKey::Character('k'), b'K' as u32),
    key_def(&["l"], "l", LogicalKey::Character('l'), b'L' as u32),
    key_def(&["m"], "m", LogicalKey::Character('m'), b'M' as u32),
    key_def(&["n"], "n", LogicalKey::Character('n'), b'N' as u32),
    key_def(&["o"], "o", LogicalKey::Character('o'), b'O' as u32),
    key_def(&["p"], "p", LogicalKey::Character('p'), b'P' as u32),
    key_def(&["q"], "q", LogicalKey::Character('q'), b'Q' as u32),
    key_def(&["r"], "r", LogicalKey::Character('r'), b'R' as u32),
    key_def(&["s"], "s", LogicalKey::Character('s'), b'S' as u32),
    key_def(&["t"], "t", LogicalKey::Character('t'), b'T' as u32),
    key_def(&["u"], "u", LogicalKey::Character('u'), b'U' as u32),
    key_def(&["v"], "v", LogicalKey::Character('v'), b'V' as u32),
    key_def(&["w"], "w", LogicalKey::Character('w'), b'W' as u32),
    key_def(&["x"], "x", LogicalKey::Character('x'), b'X' as u32),
    key_def(&["y"], "y", LogicalKey::Character('y'), b'Y' as u32),
    key_def(&["z"], "z", LogicalKey::Character('z'), b'Z' as u32),
];

/// Parses a configuration key name into its canonical [`LogicalKey`].
///
/// The chord parser lowercases tokens before calling this, so names are
/// matched as written. Accepts every row's names in [`KEY_DEFS`], the
/// historical loose function-key forms (`f01`, `f+1`), and a single ASCII
/// alphanumeric or punctuation character. Returns `None` for anything else
/// so callers can attach their own error message.
pub fn parse_key_name(value: &str) -> Option<LogicalKey> {
    let named = KEY_DEFS
        .iter()
        .find(|row| row.names.contains(&value))
        .map(|row| row.key);
    if let Some(key) = named {
        return Some(key);
    }

    if let Some(number) = value
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .filter(|number| (1..=12).contains(number))
    {
        return Some(LogicalKey::Function(number));
    }

    let mut characters = value.chars();
    if let (Some(character), None) = (characters.next(), characters.next())
        && (character.is_ascii_alphanumeric() || SINGLE_CHAR_PUNCT.contains(character))
    {
        return Some(LogicalKey::Character(character.to_ascii_lowercase()));
    }

    None
}

/// The canonical configuration spelling for `key`.
///
/// Row keys format to their canonical name, which always parses back to the
/// same key. Keys without a row — characters outside the US table, out-of-range
/// function keys, and the modifier/other sentinels — fall back to their
/// historical spellings.
pub fn logical_key_name(key: LogicalKey) -> String {
    if let Some(row) = KEY_DEFS.iter().find(|row| row.key == key) {
        return row.canonical.to_owned();
    }
    match key {
        LogicalKey::Character(character) => character.to_string(),
        LogicalKey::Function(number) => format!("f{number}"),
        LogicalKey::Modifier => "modifier".to_owned(),
        LogicalKey::Other => "other".to_owned(),
        // The completeness assertions below prove every arrow, Escape, and
        // Tab has a row, so this arm is unreachable while they hold.
        LogicalKey::Arrow(_) | LogicalKey::Escape | LogicalKey::Tab => {
            unreachable!("every arrow, Escape, and Tab has a canonical row")
        }
    }
}

/// The forward virtual-key → [`LogicalKey`] map used by the keyboard
/// normalizer.
///
/// Escape is recognized here even though its row carries no injectable
/// virtual key, because the prefix machine uses Escape for cancellation;
/// the reverse map still refuses it. Returns `None` for virtual keys outside
/// the table so callers can fall back to [`LogicalKey::Other`].
pub fn logical_key_for_vk(virtual_key: u32) -> Option<LogicalKey> {
    if virtual_key == VK_ESCAPE {
        return Some(LogicalKey::Escape);
    }
    let virtual_key = u16::try_from(virtual_key).ok()?;
    KEY_DEFS
        .iter()
        .find(|row| row.vk == Some(virtual_key))
        .map(|row| row.key)
}

/// The reverse [`LogicalKey`] → virtual-key map used to inject the literal
/// Prefix chord.
///
/// Returns `None` for keys without an injectable virtual key in the
/// canonical table: Escape (recognized for cancellation but rejected by
/// configuration), modifiers, [`LogicalKey::Other`], characters outside the
/// US-layout table, and out-of-range function keys. Callers fail closed
/// instead of guessing.
pub fn virtual_key_for_logical_key(key: LogicalKey) -> Option<u16> {
    KEY_DEFS
        .iter()
        .find(|row| row.key == key)
        .and_then(|row| row.vk)
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

const fn str_contains_char(haystack: &str, needle: char) -> bool {
    // The haystack is ASCII-only, so truncating the needle to a byte cannot
    // produce a false positive against a multi-byte character.
    let bytes = haystack.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == needle as u8 {
            return true;
        }
        index += 1;
    }
    false
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

const fn has_key(key: LogicalKey) -> bool {
    let mut index = 0;
    while index < KEY_DEFS.len() {
        if key_eq(KEY_DEFS[index].key, key) {
            return true;
        }
        index += 1;
    }
    false
}

const fn vk_eq(a: Option<u16>, b: Option<u16>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        _ => false,
    }
}

const fn has_row(key: LogicalKey, vk: Option<u16>) -> bool {
    let mut index = 0;
    while index < KEY_DEFS.len() {
        let row = &KEY_DEFS[index];
        if key_eq(row.key, key) && vk_eq(row.vk, vk) {
            return true;
        }
        index += 1;
    }
    false
}

const fn row_names_include(row: &KeyDef, name: &str) -> bool {
    let mut index = 0;
    while index < row.names.len() {
        if str_eq(row.names[index], name) {
            return true;
        }
        index += 1;
    }
    false
}

/// Compile-time consistency gate for the canonical key table.
///
/// Adding, removing, or duplicating rows in a way that would desynchronize
/// configuration parsing, display, the forward virtual-key map, or the
/// reverse injection map fails compilation here instead of surfacing as a
/// runtime inconsistency.
const _: () = {
    // Names, aliases, virtual keys, and logical keys are unique across the
    // whole table, and every canonical spelling is an accepted name.
    let mut outer = 0;
    while outer < KEY_DEFS.len() {
        let row = &KEY_DEFS[outer];
        assert!(
            !row.names.is_empty() && row_names_include(row, row.canonical),
            "every row must accept names including its canonical spelling"
        );
        let mut alias = 0;
        while alias < row.names.len() {
            let mut twin = alias + 1;
            while twin < row.names.len() {
                assert!(
                    !str_eq(row.names[alias], row.names[twin]),
                    "aliases within a row must be unique"
                );
                twin += 1;
            }
            alias += 1;
        }

        let mut inner = outer + 1;
        while inner < KEY_DEFS.len() {
            let other = &KEY_DEFS[inner];
            assert!(
                !key_eq(row.key, other.key),
                "logical keys must be unique across rows"
            );
            if let (Some(a), Some(b)) = (row.vk, other.vk) {
                assert!(a != b, "virtual keys must be unique across rows");
            }
            let mut alias = 0;
            while alias < row.names.len() {
                let mut other_alias = 0;
                while other_alias < other.names.len() {
                    assert!(
                        !str_eq(row.names[alias], other.names[other_alias]),
                        "names and aliases must be unique across rows"
                    );
                    other_alias += 1;
                }
                alias += 1;
            }
            inner += 1;
        }
        outer += 1;
    }

    // The documented non-injectable exception list is exactly one row, and
    // that row is Escape.
    let mut unrecognized = 0;
    let mut index = 0;
    while index < KEY_DEFS.len() {
        if KEY_DEFS[index].vk.is_none() {
            unrecognized += 1;
            assert!(
                key_eq(KEY_DEFS[index].key, LogicalKey::Escape),
                "only Escape may lack an injectable virtual key"
            );
        }
        index += 1;
    }
    assert!(
        unrecognized == 1,
        "exactly one row (Escape) may lack an injectable virtual key"
    );

    // Completeness: every key the forward map must recognize has a row with
    // the expected Windows virtual key, so the forward and reverse maps stay
    // total over the configurable surface.
    assert!(
        has_row(LogicalKey::Arrow(Direction::Left), Some(VK_LEFT as u16)),
        "left arrow row missing"
    );
    assert!(
        has_row(LogicalKey::Arrow(Direction::Right), Some(VK_RIGHT as u16)),
        "right arrow row missing"
    );
    assert!(
        has_row(LogicalKey::Arrow(Direction::Up), Some(VK_UP as u16)),
        "up arrow row missing"
    );
    assert!(
        has_row(LogicalKey::Arrow(Direction::Down), Some(VK_DOWN as u16)),
        "down arrow row missing"
    );
    assert!(
        has_row(LogicalKey::Tab, Some(VK_TAB as u16)),
        "tab row missing"
    );
    assert!(
        has_row(LogicalKey::Character(' '), Some(VK_SPACE as u16)),
        "space row missing"
    );

    let mut function = 0;
    while function < 12 {
        assert!(
            has_row(
                LogicalKey::Function((function + 1) as u8),
                Some((VK_F1 + function) as u16)
            ),
            "function-key rows must cover F1 through F12"
        );
        function += 1;
    }

    let mut offset: u8 = 0;
    while offset < 26 {
        assert!(
            has_row(
                LogicalKey::Character((b'a' + offset) as char),
                Some((b'A' + offset) as u16)
            ),
            "letter rows must cover a through z"
        );
        offset += 1;
    }

    let mut offset: u8 = 0;
    while offset < 10 {
        assert!(
            has_row(
                LogicalKey::Character((b'0' + offset) as char),
                Some((b'0' + offset) as u16)
            ),
            "digit rows must cover 0 through 9"
        );
        offset += 1;
    }

    assert!(
        has_row(LogicalKey::Character(';'), Some(VK_OEM_SEMICOLON as u16)),
        "semicolon row missing"
    );
    assert!(
        has_row(LogicalKey::Character('='), Some(VK_OEM_EQUALS as u16)),
        "equals row missing"
    );
    assert!(
        has_row(LogicalKey::Character(','), Some(VK_OEM_COMMA as u16)),
        "comma row missing"
    );
    assert!(
        has_row(LogicalKey::Character('-'), Some(VK_OEM_MINUS as u16)),
        "minus row missing"
    );
    assert!(
        has_row(LogicalKey::Character('.'), Some(VK_OEM_PERIOD as u16)),
        "period row missing"
    );
    assert!(
        has_row(LogicalKey::Character('/'), Some(VK_OEM_SLASH as u16)),
        "slash row missing"
    );
    assert!(
        has_row(LogicalKey::Character('`'), Some(VK_OEM_BACKTICK as u16)),
        "backtick row missing"
    );
    assert!(
        has_row(LogicalKey::Character('['), Some(VK_OEM_LEFT_BRACKET as u16)),
        "left-bracket row missing"
    );
    assert!(
        has_row(LogicalKey::Character('\\'), Some(VK_OEM_BACKSLASH as u16)),
        "backslash row missing"
    );
    assert!(
        has_row(
            LogicalKey::Character(']'),
            Some(VK_OEM_RIGHT_BRACKET as u16)
        ),
        "right-bracket row missing"
    );
    assert!(
        has_row(LogicalKey::Character('\''), Some(VK_OEM_QUOTE as u16)),
        "quote row missing"
    );

    // The historical single-character spelling set and the table's
    // punctuation rows stay in step in both directions.
    let mut index = 0;
    while index < SINGLE_CHAR_PUNCT.len() {
        let character = SINGLE_CHAR_PUNCT.as_bytes()[index] as char;
        assert!(
            has_key(LogicalKey::Character(character)),
            "every single-character spelling must have a canonical row"
        );
        index += 1;
    }

    let mut index = 0;
    while index < KEY_DEFS.len() {
        if let LogicalKey::Character(character) = KEY_DEFS[index].key {
            let letter = 'a' <= character && character <= 'z';
            let digit = '0' <= character && character <= '9';
            if !letter && !digit && character != ' ' && (character as u32) < 0x80 {
                assert!(
                    str_contains_char(SINGLE_CHAR_PUNCT, character),
                    "ASCII punctuation rows must stay single-character spellable"
                );
            }
        }
        index += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Requirement (c): every name the parser accepts resolves through a row
    /// whose virtual key exists, except the documented Escape exception.
    #[test]
    fn every_accepted_name_is_injectable_or_a_documented_exception() {
        for row in KEY_DEFS {
            for name in row.names {
                let key = parse_key_name(name).unwrap_or_else(|| panic!("{name:?} must parse"));
                assert_eq!(key, row.key, "parse({name:?})");
                assert_eq!(
                    virtual_key_for_logical_key(key).is_some(),
                    row.vk.is_some(),
                    "{name:?} must be injectable exactly when its row carries a virtual key"
                );
            }
        }

        assert_eq!(
            virtual_key_for_logical_key(parse_key_name("escape").expect("escape parses")),
            None
        );
        assert_eq!(
            virtual_key_for_logical_key(parse_key_name("esc").expect("esc parses")),
            None
        );
    }

    #[test]
    fn loose_function_key_spellings_stay_accepted_and_injectable() {
        assert_eq!(parse_key_name("f01"), Some(LogicalKey::Function(1)));
        assert_eq!(parse_key_name("f+1"), Some(LogicalKey::Function(1)));
        assert!(virtual_key_for_logical_key(LogicalKey::Function(1)).is_some());
        assert_eq!(parse_key_name("f0"), None);
        assert_eq!(parse_key_name("f13"), None);
    }

    #[test]
    fn single_character_spellings_match_the_canonical_rows() {
        for character in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
            assert_eq!(
                parse_key_name(&character.to_string()),
                Some(LogicalKey::Character(character)),
                "single-character {character:?}"
            );
        }
        assert_eq!(parse_key_name("Z"), Some(LogicalKey::Character('z')));
        for character in SINGLE_CHAR_PUNCT.chars() {
            let key = parse_key_name(&character.to_string())
                .unwrap_or_else(|| panic!("single-character {character:?} must parse"));
            assert_eq!(key, LogicalKey::Character(character));
            assert!(
                virtual_key_for_logical_key(key).is_some(),
                "single-character {character:?} must be injectable"
            );
        }
    }

    #[test]
    fn forward_map_recognizes_escape_and_rejects_unmapped_keys() {
        assert_eq!(
            logical_key_for_vk(VK_ESCAPE),
            Some(LogicalKey::Escape),
            "Escape stays recognized for prefix cancellation"
        );
        assert_eq!(
            logical_key_for_vk(0x0d),
            None,
            "unmapped keys stay unmapped"
        );
        assert_eq!(logical_key_for_vk(0x1_0000), None);
        assert_eq!(virtual_key_for_logical_key(LogicalKey::Other), None);
        assert_eq!(virtual_key_for_logical_key(LogicalKey::Modifier), None);
        assert_eq!(
            virtual_key_for_logical_key(LogicalKey::Character('A')),
            None
        );
        assert_eq!(virtual_key_for_logical_key(LogicalKey::Function(0)), None);
    }
}
