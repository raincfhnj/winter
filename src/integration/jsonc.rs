use std::collections::{HashMap, HashSet};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstNode, CstRootNode};
use serde_json::Value;

use crate::{AppError, AppResult};

use super::manifest::ManagedKeybindingManifest;
use super::{ConflictKind, IntegrationConflict};

const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone)]
pub(crate) struct DesiredKeybinding {
    pub canonical_id: String,
    pub canonical_chord: String,
    pub definition: Value,
}

#[derive(Debug)]
pub(crate) struct SettingsEdit {
    pub additions: Vec<ManagedKeybindingManifest>,
    pub conflicts: Vec<IntegrationConflict>,
    pub replacement: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct RemovalEdit {
    pub removed_binding_count: usize,
    pub preserved_binding_count: usize,
    pub retained: Vec<ManagedKeybindingManifest>,
    pub replacement: Option<Vec<u8>>,
}

/// Read-only validation and conflict analysis for the root `keybindings`
/// array: counts and conflicts without document mutation or serialization.
#[derive(Debug)]
pub struct KeybindingAnalysis {
    pub conflicts: Vec<IntegrationConflict>,
    pub existing_binding_count: usize,
    pub bindings_to_add: usize,
    pub managed_binding_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct ParsedDocument {
    root: CstRootNode,
    had_bom: bool,
}

#[derive(Debug, Clone)]
struct ExistingKeybinding {
    node: CstNode,
    canonical_id: Option<String>,
    canonical_chord: String,
    has_only_id_and_keys: bool,
}

pub(crate) fn desired_keybinding(definition: Value) -> AppResult<DesiredKeybinding> {
    let (canonical_id, canonical_chords, has_only_id_and_keys) =
        parse_binding_definition(&definition).map_err(AppError::InvalidConfiguration)?;
    let Some(canonical_id) = canonical_id else {
        return Err(AppError::InvalidConfiguration(
            "managed keybinding definitions must include an id".to_owned(),
        ));
    };
    if !has_only_id_and_keys {
        return Err(AppError::InvalidConfiguration(
            "managed keybinding definitions may contain only id and a single string keys value"
                .to_owned(),
        ));
    }
    let [canonical_chord] = canonical_chords.as_slice() else {
        return Err(AppError::InvalidConfiguration(
            "managed keybinding definitions must contain exactly one chord".to_owned(),
        ));
    };
    Ok(DesiredKeybinding {
        canonical_id,
        canonical_chord: canonical_chord.clone(),
        definition,
    })
}

pub(crate) fn validate_desired_bindings(bindings: &[DesiredKeybinding]) -> AppResult<()> {
    let mut ids = HashSet::new();
    let mut chords = HashSet::new();
    for binding in bindings {
        if !ids.insert(binding.canonical_id.clone()) {
            return Err(AppError::InvalidConfiguration(format!(
                "managed action id {} is duplicated",
                binding.canonical_id
            )));
        }
        if !chords.insert(binding.canonical_chord.clone()) {
            return Err(AppError::InvalidConfiguration(format!(
                "managed key chord {} is duplicated",
                binding.canonical_chord
            )));
        }
    }
    Ok(())
}

/// Extracts the root `keybindings` array elements together with any shape
/// conflicts that make the document unusable for merging.
fn keybinding_elements(root: &CstRootNode) -> AppResult<(Vec<CstNode>, Vec<IntegrationConflict>)> {
    let root_object = root.object_value().ok_or_else(|| {
        AppError::InvalidConfiguration("settings root must be an object".to_owned())
    })?;
    let duplicate_keybinding_properties = root_object
        .properties()
        .iter()
        .filter(|property| {
            property
                .name()
                .and_then(|name| name.decoded_value().ok())
                .as_deref()
                == Some("keybindings")
        })
        .count();
    if duplicate_keybinding_properties > 1 {
        return Ok((
            Vec::new(),
            vec![conflict(
                ConflictKind::InvalidSettingsShape,
                None,
                None,
                "settings contains duplicate root keybindings properties",
            )],
        ));
    }
    let elements = match root_object.get("keybindings") {
        Some(_) => match root_object.array_value("keybindings") {
            Some(array) => array.elements(),
            None => {
                return Ok((
                    Vec::new(),
                    vec![conflict(
                        ConflictKind::InvalidSettingsShape,
                        None,
                        None,
                        "root keybindings must be an array",
                    )],
                ));
            }
        },
        None => Vec::new(),
    };
    Ok((elements, Vec::new()))
}

struct KeybindingScan {
    existing_binding_count: usize,
    additions: Vec<ManagedKeybindingManifest>,
    matching_binding_count: usize,
    conflicts: Vec<IntegrationConflict>,
    warnings: Vec<String>,
}

/// Validates the existing `keybindings` entries against the managed set and
/// classifies every desired binding as present, conflicting, or to add.
///
/// Builds a single index pass over the existing entries so each desired
/// binding resolves in constant time.
fn scan_keybindings(elements: Vec<CstNode>, desired: &[DesiredKeybinding]) -> KeybindingScan {
    let existing_binding_count = elements.len();
    let desired_ids: HashSet<&str> = desired
        .iter()
        .map(|binding| binding.canonical_id.as_str())
        .collect();
    let (existing, mut conflicts, warnings) = parse_existing_bindings(elements, &desired_ids);

    let mut first_by_id: HashMap<&str, usize> = HashMap::with_capacity(existing.len());
    let mut first_by_chord: HashMap<&str, usize> = HashMap::with_capacity(existing.len());
    let mut existing_pairs: HashSet<(&str, &str)> = HashSet::with_capacity(existing.len());
    for (index, binding) in existing.iter().enumerate() {
        if let Some(canonical_id) = binding.canonical_id.as_deref() {
            first_by_id.entry(canonical_id).or_insert(index);
            existing_pairs.insert((canonical_id, binding.canonical_chord.as_str()));
        }
        first_by_chord
            .entry(binding.canonical_chord.as_str())
            .or_insert(index);
    }

    let mut matching_binding_count = 0;
    let mut additions = Vec::new();
    for managed in desired {
        let canonical_id = managed.canonical_id.as_str();
        let canonical_chord = managed.canonical_chord.as_str();
        if existing_pairs.contains(&(canonical_id, canonical_chord)) {
            matching_binding_count += 1;
            continue;
        }
        if first_by_id.contains_key(canonical_id) {
            conflicts.push(conflict(
                ConflictKind::SameIdDifferentBinding,
                Some(canonical_id.to_owned()),
                Some(canonical_chord.to_owned()),
                format!(
                    "managed id {canonical_id} already exists with a different chord or definition"
                ),
            ));
            continue;
        }
        if let Some(&index) = first_by_chord.get(canonical_chord) {
            let occupant = existing[index]
                .canonical_id
                .as_deref()
                .unwrap_or("an unmanaged keybinding");
            conflicts.push(conflict(
                ConflictKind::SameChordDifferentBinding,
                Some(canonical_id.to_owned()),
                Some(canonical_chord.to_owned()),
                format!("managed chord {canonical_chord} is already assigned to {occupant}"),
            ));
            continue;
        }
        additions.push(to_manifest_binding(managed));
    }
    deduplicate_conflicts(&mut conflicts);
    KeybindingScan {
        existing_binding_count,
        additions,
        matching_binding_count,
        conflicts,
        warnings,
    }
}

pub(crate) fn merge_keybindings(
    raw: &[u8],
    desired: &[DesiredKeybinding],
) -> AppResult<SettingsEdit> {
    let document = parse_document(raw)?;
    let (elements, shape_conflicts) = keybinding_elements(&document.root)?;
    if !shape_conflicts.is_empty() {
        return Ok(SettingsEdit {
            additions: Vec::new(),
            conflicts: shape_conflicts,
            replacement: None,
        });
    }
    let scan = scan_keybindings(elements, desired);
    if !scan.conflicts.is_empty() || scan.additions.is_empty() {
        return Ok(SettingsEdit {
            additions: scan.additions,
            conflicts: scan.conflicts,
            replacement: None,
        });
    }

    let root_object = document.root.object_value().ok_or_else(|| {
        AppError::InvalidConfiguration("settings root must be an object".to_owned())
    })?;
    let array = root_object
        .array_value_or_create("keybindings")
        .ok_or_else(|| AppError::InvalidConfiguration("keybindings is not an array".to_owned()))?;
    for addition in &scan.additions {
        array.append(value_to_cst(&addition.definition)?);
    }
    let replacement = serialize_document(&document);
    Ok(SettingsEdit {
        additions: scan.additions,
        conflicts: scan.conflicts,
        replacement: Some(replacement),
    })
}

/// Validates and analyzes the root `keybindings` array without mutating or
/// serializing the settings document, so read-only commands skip the full
/// document re-serialization that [`merge_keybindings`] pays when it appends
/// bindings.
pub fn analyze_keybindings(
    raw: &[u8],
    desired: &[DesiredKeybinding],
) -> AppResult<KeybindingAnalysis> {
    let document = parse_document(raw)?;
    let (elements, shape_conflicts) = keybinding_elements(&document.root)?;
    if !shape_conflicts.is_empty() {
        return Ok(KeybindingAnalysis {
            conflicts: shape_conflicts,
            existing_binding_count: 0,
            bindings_to_add: 0,
            managed_binding_count: 0,
            warnings: Vec::new(),
        });
    }
    let scan = scan_keybindings(elements, desired);
    Ok(KeybindingAnalysis {
        existing_binding_count: scan.existing_binding_count,
        bindings_to_add: scan.additions.len(),
        managed_binding_count: scan.matching_binding_count,
        conflicts: scan.conflicts,
        warnings: scan.warnings,
    })
}

pub(crate) fn remove_managed_keybindings(
    raw: &[u8],
    managed: &[ManagedKeybindingManifest],
) -> AppResult<RemovalEdit> {
    let document = parse_document(raw)?;
    let root_object = document.root.object_value().ok_or_else(|| {
        AppError::InvalidConfiguration("settings root must be an object".to_owned())
    })?;
    let Some(array) = root_object.array_value("keybindings") else {
        if root_object.get("keybindings").is_some() {
            return Err(AppError::InvalidConfiguration(
                "root keybindings must be an array".to_owned(),
            ));
        }
        return Ok(RemovalEdit {
            removed_binding_count: 0,
            preserved_binding_count: 0,
            retained: Vec::new(),
            replacement: None,
        });
    };
    let managed_ids: HashSet<&str> = managed
        .iter()
        .map(|record| record.canonical_id.as_str())
        .collect();
    let (existing, parse_conflicts, _warnings) =
        parse_existing_bindings(array.elements(), &managed_ids);
    if !parse_conflicts.is_empty() {
        return Err(AppError::SettingsConflict(
            "settings contains malformed keybindings; managed entries were retained".to_owned(),
        ));
    }

    let mut consumed = HashSet::new();
    let mut nodes_to_remove = Vec::new();
    let mut retained = Vec::new();
    for record in managed {
        let expected = desired_keybinding(record.definition.clone())?;
        let exact = existing.iter().enumerate().find(|(index, binding)| {
            !consumed.contains(index)
                && binding.canonical_id.as_deref() == Some(expected.canonical_id.as_str())
                && binding.canonical_chord == expected.canonical_chord
                && binding.has_only_id_and_keys
        });
        if let Some((index, binding)) = exact {
            consumed.insert(index);
            nodes_to_remove.push(binding.node.clone());
            continue;
        }

        let was_modified = existing.iter().any(|binding| {
            binding.canonical_id.as_deref() == Some(record.canonical_id.as_str())
                || binding.canonical_chord == record.canonical_chord
        });
        if was_modified {
            retained.push(record.clone());
        }
    }

    let removed_binding_count = nodes_to_remove.len();
    for node in nodes_to_remove {
        node.remove();
    }
    let replacement = (removed_binding_count > 0).then(|| serialize_document(&document));
    Ok(RemovalEdit {
        removed_binding_count,
        preserved_binding_count: retained.len(),
        retained,
        replacement,
    })
}

fn parse_document(raw: &[u8]) -> AppResult<ParsedDocument> {
    let (had_bom, source) = match raw.strip_prefix(UTF8_BOM) {
        Some(source) => (true, source),
        None => (false, raw),
    };
    let source = std::str::from_utf8(source).map_err(|error| {
        AppError::InvalidConfiguration(format!("settings is not valid UTF-8: {error}"))
    })?;
    let options = ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    };
    let root = CstRootNode::parse(source, &options).map_err(|error| {
        AppError::InvalidConfiguration(format!("settings JSONC could not be parsed: {error}"))
    })?;
    Ok(ParsedDocument { root, had_bom })
}

fn serialize_document(document: &ParsedDocument) -> Vec<u8> {
    let serialized = document.root.to_string();
    let mut bytes = Vec::with_capacity(serialized.len() + usize::from(document.had_bom) * 3);
    if document.had_bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(serialized.as_bytes());
    bytes
}

/// Parses every root `keybindings` element, splitting them into usable
/// bindings, malformed entries that belong to a managed action (a conflict,
/// because merging must not clobber them), and warnings for malformed entries
/// outside the managed set, which are inert and skipped.
fn parse_existing_bindings(
    elements: Vec<CstNode>,
    desired_ids: &HashSet<&str>,
) -> (
    Vec<ExistingKeybinding>,
    Vec<IntegrationConflict>,
    Vec<String>,
) {
    let mut parsed = Vec::new();
    let mut conflicts = Vec::new();
    let mut warnings = Vec::new();
    for node in elements {
        let Some(value) = node.to_serde_value() else {
            warnings.push(
                "ignored unmanaged keybinding: value cannot be represented as JSON".to_owned(),
            );
            continue;
        };
        match parse_binding_definition(&value) {
            Ok((canonical_id, canonical_chords, has_only_id_and_keys)) => {
                for canonical_chord in canonical_chords {
                    parsed.push(ExistingKeybinding {
                        node: node.clone(),
                        canonical_id: canonical_id.clone(),
                        canonical_chord,
                        has_only_id_and_keys,
                    });
                }
            }
            Err(message) => {
                if references_desired_id(&value, desired_ids) {
                    conflicts.push(conflict(
                        ConflictKind::MalformedKeybinding,
                        None,
                        None,
                        message,
                    ));
                } else {
                    warnings.push(format!("ignored unmanaged keybinding: {message}"));
                }
            }
        }
    }
    (parsed, conflicts, warnings)
}

/// Whether the entry declares one of the managed action ids, even when the
/// rest of the entry fails to parse.
fn references_desired_id(value: &Value, desired_ids: &HashSet<&str>) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| normalize_id(id).ok())
        .is_some_and(|id| desired_ids.contains(id.as_str()))
}

/// Parses one root `keybindings` entry.
///
/// Returns the optional managed `id`, every chord declared by `keys` (a string
/// or an array of strings), and whether the entry is a bare `{id, keys}` object
/// with a single string chord that WinTerminalP is allowed to remove.
fn parse_binding_definition(value: &Value) -> Result<(Option<String>, Vec<String>, bool), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "each root keybindings entry must be an object".to_owned())?;
    let canonical_id = match object.get("id") {
        Some(Value::String(id)) => Some(normalize_id(id)?),
        Some(_) => return Err("keybinding id must be a string".to_owned()),
        None => None,
    };
    let keys = object.get("keys").ok_or_else(|| match &canonical_id {
        Some(id) => format!("keybinding {id} is missing keys"),
        None => "keybinding entry is missing keys".to_owned(),
    })?;
    let (canonical_chords, keys_is_string) = match keys {
        Value::String(keys) => (vec![normalize_chord(keys)?], true),
        Value::Array(keys) => {
            let mut chords = Vec::with_capacity(keys.len());
            for entry in keys {
                let chord = entry
                    .as_str()
                    .ok_or_else(|| "keybinding keys array must contain only strings".to_owned())?;
                chords.push(normalize_chord(chord)?);
            }
            if chords.is_empty() {
                return Err("keybinding keys array cannot be empty".to_owned());
            }
            (chords, false)
        }
        _ => return Err("keybinding has an invalid keys value".to_owned()),
    };
    let has_only_id_and_keys = canonical_id.is_some() && object.len() == 2 && keys_is_string;
    Ok((canonical_id, canonical_chords, has_only_id_and_keys))
}

fn normalize_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("keybinding id cannot be empty".to_owned());
    }
    Ok(id.to_ascii_lowercase())
}

/// Normalizes a chord to canonical `modifier+...+key` order.
///
/// A run of empty parts produced by splitting on `+` means the literal `+`
/// key (`ctrl++`, `+`), which canonicalizes to `plus` (VK_OEM_PLUS).
fn normalize_chord(chord: &str) -> Result<String, String> {
    let chord = chord.trim().to_ascii_lowercase();
    if chord.is_empty() {
        return Err("keybinding chord cannot be empty".to_owned());
    }
    let mut modifiers = HashSet::new();
    let mut key = None;
    let mut previous_part_was_empty = false;
    for part in chord.split('+').map(str::trim) {
        if part.is_empty() {
            if previous_part_was_empty {
                continue;
            }
            previous_part_was_empty = true;
            if key.replace("plus").is_some() {
                return Err(format!("keybinding chord {chord} contains multiple keys"));
            }
            continue;
        }
        previous_part_was_empty = false;
        let normalized = normalize_chord_token(part);
        if matches!(normalized, "ctrl" | "shift" | "alt" | "win") {
            if !modifiers.insert(normalized) {
                return Err(format!("duplicate modifier in keybinding chord {chord}"));
            }
        } else if key.replace(normalized).is_some() {
            return Err(format!("keybinding chord {chord} contains multiple keys"));
        }
    }
    let key = key.ok_or_else(|| format!("keybinding chord {chord} has no key"))?;
    let mut canonical = Vec::new();
    for modifier in ["ctrl", "shift", "alt", "win"] {
        if modifiers.contains(modifier) {
            canonical.push(modifier);
        }
    }
    canonical.push(key);
    Ok(canonical.join("+"))
}

/// Canonicalizes one already-lowercased chord token without allocating.
fn normalize_chord_token(part: &str) -> &str {
    match part {
        "control" => "ctrl",
        "windows" => "win",
        "escape" => "esc",
        "return" => "enter",
        "pageup" => "pgup",
        "pagedown" => "pgdn",
        "oemplus" => "plus",
        other => other,
    }
}

fn to_manifest_binding(binding: &DesiredKeybinding) -> ManagedKeybindingManifest {
    ManagedKeybindingManifest {
        canonical_id: binding.canonical_id.clone(),
        canonical_chord: binding.canonical_chord.clone(),
        definition: binding.definition.clone(),
    }
}

fn value_to_cst(value: &Value) -> AppResult<CstInputValue> {
    match value {
        Value::Null => Ok(CstInputValue::Null),
        Value::Bool(value) => Ok(CstInputValue::Bool(*value)),
        Value::Number(value) => Ok(CstInputValue::Number(value.to_string())),
        Value::String(value) => Ok(CstInputValue::String(value.clone())),
        Value::Array(values) => values
            .iter()
            .map(value_to_cst)
            .collect::<AppResult<Vec<_>>>()
            .map(CstInputValue::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), value_to_cst(value)?)))
            .collect::<AppResult<Vec<_>>>()
            .map(CstInputValue::Object),
    }
}

fn conflict(
    kind: ConflictKind,
    action_id: Option<String>,
    keys: Option<String>,
    message: impl Into<String>,
) -> IntegrationConflict {
    IntegrationConflict {
        kind,
        action_id,
        keys,
        message: message.into(),
    }
}

fn deduplicate_conflicts(conflicts: &mut Vec<IntegrationConflict>) {
    let unique: Vec<bool> = {
        let mut seen = HashSet::new();
        conflicts
            .iter()
            .map(|conflict| {
                seen.insert((
                    conflict.kind,
                    conflict.action_id.as_deref(),
                    conflict.keys.as_deref(),
                    conflict.message.as_str(),
                ))
            })
            .collect()
    };
    let mut index = 0;
    conflicts.retain(|_| {
        let keep = unique[index];
        index += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn desired(id: &str, keys: &str) -> DesiredKeybinding {
        desired_keybinding(json!({ "id": id, "keys": keys }))
            .expect("managed fixture should be valid")
    }

    #[test]
    fn merge_is_lossless_around_the_edited_array_and_idempotent() {
        let source = b"\xef\xbb\xbf{\r\n  // keep this comment\r\n  \"profiles\": [],\r\n}\r\n";
        let bindings = [desired("WinTerminalP.SplitLeft", "ctrl+f13")];

        let first = merge_keybindings(source, &bindings).expect("merge should succeed");
        assert!(first.conflicts.is_empty());
        let replacement = first.replacement.expect("binding should be appended");
        assert!(replacement.starts_with(UTF8_BOM));
        let text = std::str::from_utf8(&replacement[3..]).expect("result should be UTF-8");
        assert!(text.contains("// keep this comment\r\n"));
        assert!(text.contains("\"profiles\": []"));

        let second = merge_keybindings(&replacement, &bindings).expect("second merge should work");
        assert!(second.conflicts.is_empty());
        assert!(second.replacement.is_none());
        let second_analysis =
            analyze_keybindings(&replacement, &bindings).expect("second analysis should work");
        assert_eq!(second_analysis.managed_binding_count, 1);
    }

    #[test]
    fn blocks_same_id_with_a_different_chord() {
        let source = br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f14"}]}"#;
        let result = merge_keybindings(source, &[desired("WinTerminalP.SplitLeft", "ctrl+f13")])
            .expect("analysis should complete");

        assert!(result.replacement.is_none());
        assert!(
            result
                .conflicts
                .iter()
                .any(|conflict| { conflict.kind == ConflictKind::SameIdDifferentBinding })
        );
    }

    #[test]
    fn blocks_same_chord_with_a_different_id_after_normalization() {
        let source = br#"{"keybindings":[{"id":"User.Action","keys":"SHIFT + CTRL + F13"}]}"#;
        let result = merge_keybindings(
            source,
            &[desired("WinTerminalP.SplitLeft", "ctrl+shift+f13")],
        )
        .expect("analysis should complete");

        assert!(result.replacement.is_none());
        assert!(
            result
                .conflicts
                .iter()
                .any(|conflict| { conflict.kind == ConflictKind::SameChordDifferentBinding })
        );
    }

    #[test]
    fn uninstall_removes_only_semantically_unchanged_manifest_entries() {
        let source = br#"{
  "keybindings": [
    { "id": "WinTerminalP.SplitLeft", "keys": "ctrl+f13" },
    { "id": "WinTerminalP.SplitRight", "keys": "ctrl+f24", "userNote": true },
    { "id": "User.Action", "keys": "ctrl+x" }
  ]
}"#;
        let records = [
            to_manifest_binding(&desired("WinTerminalP.SplitLeft", "ctrl+f13")),
            to_manifest_binding(&desired("WinTerminalP.SplitRight", "ctrl+f14")),
        ];

        let edit = remove_managed_keybindings(source, &records).expect("removal should succeed");
        let replacement = edit.replacement.expect("one binding should be removed");
        let text = std::str::from_utf8(&replacement).expect("result should be UTF-8");

        assert_eq!(edit.removed_binding_count, 1);
        assert_eq!(edit.preserved_binding_count, 1);
        assert!(!text.contains("SplitLeft"));
        assert!(text.contains("SplitRight"));
        assert!(text.contains("User.Action"));
    }

    #[test]
    fn command_keybindings_without_id_are_not_malformed() {
        let source = br#"{"keybindings":[{"command":"newTab","keys":"ctrl+shift+t"}]}"#;
        let result = merge_keybindings(
            source,
            &[desired("WinTerminalP.SplitLeft", "ctrl+alt+shift+f13")],
        )
        .expect("analysis should complete");

        assert!(result.conflicts.is_empty());
        assert!(result.replacement.is_some());
    }

    #[test]
    fn multi_chord_entries_are_not_malformed_but_still_reserve_their_chords() {
        let source =
            br#"{"keybindings":[{"id":"User.Multi","keys":["ctrl+alt+shift+f13","ctrl+x"]}]}"#;
        let result = merge_keybindings(
            source,
            &[desired("WinTerminalP.SplitLeft", "ctrl+alt+shift+f13")],
        )
        .expect("analysis should complete");

        assert!(
            result
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::SameChordDifferentBinding)
        );
        assert!(
            !result
                .conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::MalformedKeybinding)
        );
    }

    #[test]
    fn analyze_matches_merge_counts_and_conflicts() {
        let desired_bindings = [
            desired("WinTerminalP.SplitLeft", "ctrl+f13"),
            desired("WinTerminalP.SplitRight", "ctrl+f14"),
        ];
        let fixtures: &[&[u8]] = &[
            br#"{}"#,
            br#"{"keybindings":[]}"#,
            br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f13"}]}"#,
            br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f14"},{"id":"User.Action","keys":"ctrl+alt+f14"}]}"#,
            br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f13","command":"unbound"}]}"#,
            br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft"},{"id":"User.Junk","keys":[]}]}"#,
            br#"{"keybindings":[42,"junk"]}"#,
            br#"{ "keybindings": [], "keybindings": [] }"#,
            br#"{"keybindings":{}}"#,
        ];
        for source in fixtures {
            let merged =
                merge_keybindings(source, &desired_bindings).expect("fixture should merge");
            let analyzed =
                analyze_keybindings(source, &desired_bindings).expect("fixture should analyze");
            assert_eq!(
                analyzed.bindings_to_add,
                merged.additions.len(),
                "additions for {source:?}"
            );
            assert_eq!(
                analyzed.conflicts, merged.conflicts,
                "conflicts for {source:?}"
            );
        }

        assert!(merge_keybindings(b"[1]", &desired_bindings).is_err());
        assert!(analyze_keybindings(b"[1]", &desired_bindings).is_err());
        assert!(merge_keybindings(b"{", &desired_bindings).is_err());
        assert!(analyze_keybindings(b"{", &desired_bindings).is_err());
    }

    #[test]
    fn unmanaged_malformed_entries_warn_instead_of_blocking() {
        let source = br#"{
  "keybindings": [
    42,
    "nonsense",
    { "command": "x" },
    { "id": "User.Junk" },
    { "id": "User.Junk", "keys": [] },
    { "id": "User.Junk", "keys": 7 }
  ]
}"#;
        let bindings = [desired("WinTerminalP.SplitLeft", "ctrl+f13")];

        let edit =
            merge_keybindings(source, &bindings).expect("unmanaged junk must not block install");
        assert!(edit.conflicts.is_empty());
        let replacement = edit.replacement.expect("binding should be appended");
        let text = std::str::from_utf8(&replacement).expect("result should be UTF-8");
        assert!(text.contains("\"nonsense\""));

        let analysis = analyze_keybindings(source, &bindings).expect("analysis should complete");
        assert_eq!(analysis.existing_binding_count, 6);
        assert_eq!(analysis.managed_binding_count, 0);
        assert_eq!(analysis.warnings.len(), 6);
        assert!(
            analysis
                .warnings
                .iter()
                .all(|warning| warning.starts_with("ignored unmanaged keybinding: "))
        );
        assert_eq!(analysis.bindings_to_add, 1);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn malformed_managed_entries_still_conflict() {
        let source = br#"{
  "keybindings": [
    { "id": "WinTerminalP.SplitLeft" },
    { "id": "WinTerminalP.SplitRight", "keys": [] },
    { "id": "WinTerminalP.SplitRight", "keys": 7 },
    { "id": "User.Junk" }
  ]
}"#;
        let bindings = [
            desired("WinTerminalP.SplitLeft", "ctrl+f13"),
            desired("WinTerminalP.SplitRight", "ctrl+f14"),
        ];

        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert!(edit.replacement.is_none());
        assert_eq!(
            edit.conflicts
                .iter()
                .filter(|conflict| conflict.kind == ConflictKind::MalformedKeybinding)
                .count(),
            3
        );

        let analysis = analyze_keybindings(source, &bindings).expect("analysis should complete");
        assert_eq!(analysis.conflicts.len(), 3);
        assert_eq!(analysis.warnings.len(), 1);
    }

    #[test]
    fn literal_plus_chords_normalize_to_the_plus_key() {
        assert_eq!(
            normalize_chord("ctrl++").expect("ctrl++ should parse"),
            "ctrl+plus"
        );
        assert_eq!(normalize_chord("+").expect("+ should parse"), "plus");
        assert_eq!(
            normalize_chord("ctrl+shift++").expect("ctrl+shift++ should parse"),
            "ctrl+shift+plus"
        );
        assert_eq!(
            normalize_chord("Ctrl+OEMPLUS").expect("oemplus alias should parse"),
            "ctrl+plus"
        );
        assert_eq!(
            normalize_chord("ctrl+plus").expect("ctrl+plus should parse"),
            "ctrl+plus"
        );
        assert!(normalize_chord("ctrl++ctrl").is_err());
        assert!(normalize_chord("+a+").is_err());
    }

    #[test]
    fn ctrl_plus_plus_collides_with_the_canonical_plus_chord() {
        let source = br#"{"keybindings":[{"id":"User.Plus","keys":"ctrl++"}]}"#;
        let bindings = [desired("WinTerminalP.SplitLeft", "ctrl+plus")];

        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert!(edit.replacement.is_none());
        assert!(
            edit.conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::SameChordDifferentBinding)
        );
    }

    #[test]
    fn extra_properties_do_not_block_a_managed_entry_with_matching_keys() {
        let bindings = [desired("WinTerminalP.SplitLeft", "ctrl+f13")];
        let source = br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f13","command":"unbound","userNote":true}]}"#;

        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert!(edit.conflicts.is_empty());
        assert_eq!(edit.additions.len(), 0);
        assert!(edit.replacement.is_none());
        let analysis = analyze_keybindings(source, &bindings).expect("analysis should complete");
        assert_eq!(analysis.managed_binding_count, 1);
        assert_eq!(analysis.bindings_to_add, 0);

        let source =
            br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f14","command":"unbound"}]}"#;
        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert!(edit.replacement.is_none());
        assert_eq!(
            edit.conflicts
                .iter()
                .filter(|conflict| conflict.kind == ConflictKind::SameIdDifferentBinding)
                .count(),
            1
        );
    }

    #[test]
    fn index_lookup_preserves_first_match_selection() {
        let bindings = [desired("WinTerminalP.SplitLeft", "ctrl+f13")];

        let source = br#"{"keybindings":[
            {"id":"WinTerminalP.SplitLeft","keys":"ctrl+f14"},
            {"id":"WinTerminalP.SplitLeft","keys":"ctrl+f13"}
        ]}"#;
        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert!(edit.conflicts.is_empty());
        assert!(edit.replacement.is_none());
        let analysis = analyze_keybindings(source, &bindings).expect("analysis should complete");
        assert_eq!(analysis.managed_binding_count, 1);

        let source = br#"{"keybindings":[
            {"id":"WinTerminalP.SplitLeft","keys":"ctrl+f15"},
            {"id":"WinTerminalP.SplitLeft","keys":"ctrl+f16"}
        ]}"#;
        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        assert_eq!(
            edit.conflicts
                .iter()
                .filter(|conflict| conflict.kind == ConflictKind::SameIdDifferentBinding)
                .count(),
            1
        );

        let source = br#"{"keybindings":[
            {"id":"User.First","keys":"ctrl+f13"},
            {"id":"User.Second","keys":"CTRL + F13"}
        ]}"#;
        let edit = merge_keybindings(source, &bindings).expect("analysis should complete");
        let conflict = edit
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == ConflictKind::SameChordDifferentBinding)
            .expect("chord conflict should be reported");
        assert_eq!(
            conflict.action_id.as_deref(),
            Some("winterminalp.splitleft")
        );
        assert!(
            conflict.message.contains("user.first"),
            "message was: {}",
            conflict.message
        );
        assert!(!conflict.message.contains("user.second"));
    }

    #[test]
    fn removal_skips_unmanaged_junk_but_still_blocks_managed_malformed_entries() {
        let records = [to_manifest_binding(&desired(
            "WinTerminalP.SplitLeft",
            "ctrl+f13",
        ))];
        let source =
            br#"{"keybindings":[{"id":"User.Junk"},{"id":"WinTerminalP.SplitLeft","keys":"ctrl+f13"}]}"#;
        let edit = remove_managed_keybindings(source, &records)
            .expect("unmanaged junk must not block removal");
        assert_eq!(edit.removed_binding_count, 1);
        let replacement = edit.replacement.expect("one binding should be removed");
        let text = std::str::from_utf8(&replacement).expect("result should be UTF-8");
        assert!(text.contains("User.Junk"));
        assert!(!text.contains("SplitLeft"));

        let source = br#"{"keybindings":[{"id":"WinTerminalP.SplitLeft"}]}"#;
        assert!(remove_managed_keybindings(source, &records).is_err());
    }
}
