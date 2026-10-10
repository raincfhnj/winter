//! Identity of the account this process runs as.
//!
//! Autostart needs a stable principal for the scheduled task: a string SID is
//! used instead of `DOMAIN\User`, because it survives account renames and is
//! the only spelling that is unambiguous for local, Microsoft, and directory
//! accounts alike.

use std::mem::size_of;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::Owned;

use super::error::{PlatformError, PlatformResult};

/// Returns the string SID of the account owning this process's token.
pub fn current_user_sid_string() -> PlatformResult<String> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle refers to this process, TOKEN_QUERY is
    // read-only, and `token` is a writable out parameter.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|source| PlatformError::win32("OpenProcessToken", source))?;
    // SAFETY: OpenProcessToken returned a newly-owned valid handle.
    let token = unsafe { Owned::new(token) };

    // SAFETY: a null buffer with length 0 is the documented sizing call; the
    // function only writes the required length into `needed`.
    let mut needed = 0_u32;
    let _ = unsafe { GetTokenInformation(*token, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(PlatformError::CurrentUserSid(
            "Windows reported an empty token-user structure".to_owned(),
        ));
    }

    let mut buffer = vec![0_u8; needed as usize];
    // SAFETY: `buffer` is exactly `needed` bytes and stays alive and uniquely
    // borrowed for the duration of the call.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|source| PlatformError::win32("GetTokenInformation(TokenUser)", source))?;

    // SAFETY: GetTokenInformation filled the buffer with a TOKEN_USER whose
    // Sid pointer stays valid as long as `buffer` does.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: the SID lives inside the token buffer validated above.
    let sid = unsafe {
        std::slice::from_raw_parts(
            user.User.Sid.0.cast::<u8>(),
            sid_length(user.User.Sid.0.cast::<u8>())?,
        )
    };

    sid_to_string(sid).ok_or_else(|| {
        PlatformError::CurrentUserSid("the token SID is not a well-formed SID".to_owned())
    })
}

/// Reads the byte length of a SID from its own header.
///
/// A SID is `S-` plus a revision byte, a sub-authority count, a six-byte
/// identifier authority, and `count` little-endian 32-bit sub-authorities.
///
/// # Safety
///
/// `sid` must point at a valid SID header with at least eight readable bytes.
unsafe fn sid_length(sid: *const u8) -> PlatformResult<usize> {
    // SAFETY: the caller guarantees eight readable header bytes.
    let header = unsafe { std::slice::from_raw_parts(sid, 8) };
    Ok(8 + usize::from(header[1]) * size_of::<u32>())
}

/// Renders a SID byte image as the canonical `S-1-5-21-…` string.
///
/// Returns `None` when the bytes are too short for the sub-authority count
/// they declare.
#[must_use]
pub fn sid_to_string(bytes: &[u8]) -> Option<String> {
    let revision = *bytes.first()?;
    let sub_authority_count = usize::from(*bytes.get(1)?);
    let authority = bytes.get(2..8)?;
    let length = 8 + sub_authority_count * size_of::<u32>();
    if bytes.len() < length {
        return None;
    }

    let mut identifier_authority = 0_u64;
    for byte in authority {
        identifier_authority = (identifier_authority << 8) | u64::from(*byte);
    }

    let mut rendered = format!("S-{revision}-{identifier_authority}");
    for index in 0..sub_authority_count {
        let start = 8 + index * size_of::<u32>();
        let sub_authority = u32::from_le_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]);
        rendered.push_str(&format!("-{sub_authority}"));
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_canonical_string_form() {
        // S-1-5-21-4209845023-1725502529-2735297935-500, the local
        // Administrator account of a default Windows installation.
        let mut sid = vec![1, 5];
        sid.extend_from_slice(&[0, 0, 0, 0, 0, 5]);
        for sub in [21_u32, 4_209_845_023, 1_725_502_529, 2_735_297_935, 500] {
            sid.extend_from_slice(&sub.to_le_bytes());
        }

        assert_eq!(
            sid_to_string(&sid).as_deref(),
            Some("S-1-5-21-4209845023-1725502529-2735297935-500")
        );
    }

    #[test]
    fn renders_well_known_short_sids() {
        // S-1-5-18 (LocalSystem): revision 1, one sub-authority, authority 5.
        assert_eq!(
            sid_to_string(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]).as_deref(),
            Some("S-1-5-18")
        );
    }

    #[test]
    fn rejects_truncated_sids() {
        assert_eq!(sid_to_string(&[]), None);
        assert_eq!(sid_to_string(&[1]), None);
        assert_eq!(sid_to_string(&[1, 4, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]), None);
    }

    /// The live lookup is the value the scheduled task is registered with, so
    /// it must produce a SID this machine can resolve.
    #[test]
    fn the_current_account_has_a_string_sid() {
        let sid = current_user_sid_string().expect("the token must expose a user SID");
        assert!(sid.starts_with("S-1-"), "unexpected SID: {sid}");
        assert!(
            sid.len() > 5 && sid[4..].chars().all(|c| c.is_ascii_digit() || c == '-'),
            "unexpected SID: {sid}"
        );
    }
}
