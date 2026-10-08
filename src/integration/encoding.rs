//! Byte-level text encoding helpers shared by settings and profile edits.
//!
//! Windows Terminal `settings.json` and the PowerShell profiles are both
//! round-tripped in whatever encoding the user's editor saved them with, so
//! the UTF-16LE primitives live here and each caller maps the shared failure
//! modes onto its own file-specific error type.

pub(crate) const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
pub(crate) const UTF16LE_BOM: &[u8] = b"\xff\xfe";
pub(crate) const UTF16BE_BOM: &[u8] = b"\xfe\xff";

/// Failure modes of [`decode_utf16_le`], so each caller can phrase its own
/// file-specific message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Utf16LeError {
    Truncated,
    Invalid,
}

/// Decodes a UTF-16LE payload whose leading `FF FE` byte-order mark has
/// already been stripped.
pub(crate) fn decode_utf16_le(content: &[u8]) -> Result<String, Utf16LeError> {
    if !content.len().is_multiple_of(2) {
        return Err(Utf16LeError::Truncated);
    }
    let units = content
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| Utf16LeError::Invalid)
}

/// Encodes `text` as UTF-16LE prefixed with the `FF FE` byte-order mark.
pub(crate) fn encode_utf16_le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2 + UTF16LE_BOM.len());
    bytes.extend_from_slice(UTF16LE_BOM);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16le_round_trip_preserves_non_ascii_and_crlf() {
        let original = "{\r\n  // é comment\r\n  \"k\": \"中文\"\r\n}\r\n";
        let bytes = encode_utf16_le_with_bom(original);
        assert_eq!(&bytes[..2], UTF16LE_BOM);
        assert!(bytes.len().is_multiple_of(2));
        let decoded = decode_utf16_le(&bytes[UTF16LE_BOM.len()..]).expect("payload should decode");
        assert_eq!(decoded, original);
        assert_eq!(encode_utf16_le_with_bom(&decoded), bytes);
    }

    #[test]
    fn truncated_and_invalid_utf16le_payloads_are_classified() {
        assert_eq!(
            decode_utf16_le(&[0x7b, 0x00, 0x7d]),
            Err(Utf16LeError::Truncated)
        );
        // A lone high surrogate without its low surrogate.
        assert_eq!(
            decode_utf16_le(&0xd800u16.to_le_bytes()),
            Err(Utf16LeError::Invalid)
        );
    }
}
