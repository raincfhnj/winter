//! Location proof for the executable a logon task starts.
//!
//! Registering the Winter logon task needs administrator approval exactly once,
//! but the program the task *runs* needs none at all: Task Scheduler starts
//! `winterd.exe` at the highest available run level at every sign-in. If an
//! unprivileged process can replace that file — or replace any directory on the
//! way to it — then "install once, run elevated at every logon" turns into a
//! persistence primitive for whoever can write there. The default Cargo install
//! location, `%USERPROFILE%\.cargo\bin`, is writable by the account itself, so
//! that is not a theoretical concern.
//!
//! This module answers one question before the task is registered: can a
//! medium-integrity process running as this account modify the executable, or
//! remove, rename, re-permission, or re-own any entry on the path to it?
//!
//! # What is checked
//!
//! * the executable itself — any access that lets a principal overwrite its
//!   contents, delete it, or rewrite its permissions;
//! * every ancestor directory up to the volume root — any access that lets a
//!   principal delete that directory, delete the child on our path, or rewrite
//!   its permissions or owner. `FILE_WRITE_DATA`/`FILE_APPEND_DATA` alone on an
//!   *ancestor* is deliberately not enough: it only permits creating a new
//!   sibling entry, and the entries on our path already exist. Accepting that
//!   distinction is what keeps the volume root (`C:\` grants
//!   `Authenticated Users:(AD)`, append data, by design) from invalidating
//!   every path on the drive, while `C:\Users\<account>` — which grants the
//!   account full control — is still refused.
//!
//! # Who counts as unsafe
//!
//! A grant is fatal when its trustee is a principal that a medium-integrity
//! process of this account can present:
//!
//! * the well-known "any interactive user" SIDs (Everyone, Authenticated Users,
//!   `BUILTIN\Users`, INTERACTIVE, CONSOLE LOGON, ANONYMOUS LOGON, ALL
//!   APPLICATION PACKAGES);
//! * the account this process runs as, taken from the access token;
//! * any group the token holds *enabled* (a group carried only to deny, which is
//!   where an unelevated administrator token keeps `BUILTIN\Administrators`, is
//!   not usable and is therefore ignored).
//!
//! `BUILTIN\Administrators`, `NT AUTHORITY\SYSTEM`, the service accounts, and
//! `NT SERVICE\TrustedInstaller` are explicitly *not* unsafe trustees: a
//! medium-integrity process cannot use them, and treating them as unsafe would
//! reject `%ProgramFiles%`, whose DACL grants both of them write access. That
//! exemption is load-bearing, not cosmetic.
//!
//! # Failure direction
//!
//! Every ambiguity is resolved towards refusal: an unreadable descriptor, a
//! DACL that is absent (which grants every account full control), an ACE the
//! walk cannot interpret, or an ACE type Windows has added since this code was
//! written all refuse the location. The module never reports "protected" for a
//! path it could not fully inspect.

use std::env;
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::path::{Path, PathBuf};
use std::ptr::read_unaligned;

use windows::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, HANDLE};
use windows::Win32::Security::{
    ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT, ACL, ACL_SIZE_INFORMATION,
    AclSizeInformation, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
    GetFileSecurityW, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
    INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
    TOKEN_GROUPS, TOKEN_QUERY, TokenGroups, WELL_KNOWN_SID_TYPE, WinAnonymousSid,
    WinAuthenticatedUserSid, WinBuiltinAdministratorsSid, WinBuiltinAnyPackageSid,
    WinBuiltinUsersSid, WinConsoleLogonSid, WinInteractiveSid, WinLocalServiceSid,
    WinLocalSystemSid, WinNetworkServiceSid, WinWorldSid,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_APPEND_DATA, FILE_DELETE_CHILD,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
    ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_OBJECT_ACE_TYPE,
    ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE, SE_GROUP_ENABLED,
    SE_GROUP_USE_FOR_DENY_ONLY, SYSTEM_ACCESS_FILTER_ACE_TYPE, SYSTEM_ALARM_ACE_TYPE,
    SYSTEM_ALARM_CALLBACK_ACE_TYPE, SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE,
    SYSTEM_ALARM_OBJECT_ACE_TYPE, SYSTEM_AUDIT_ACE_TYPE, SYSTEM_AUDIT_CALLBACK_ACE_TYPE,
    SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE, SYSTEM_AUDIT_OBJECT_ACE_TYPE,
    SYSTEM_MANDATORY_LABEL_ACE_TYPE, SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE,
    SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE, SYSTEM_SCOPED_POLICY_ID_ACE_TYPE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, Owned, PCWSTR};

use super::error::{PlatformError, PlatformResult};
use super::identity::{current_user_sid_string, sid_to_string};

/// `SECURITY_MAX_SID_SIZE`: the documented upper bound on a SID image.
const MAX_SID_SIZE: usize = 68;

/// What a grant lets a medium-integrity process do to the path under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Overwrite the executable, or rewrite its permissions.
    Overwrite,
    /// Delete, rename, or re-permission the directory that holds the next
    /// element of the path, which removes the executable with it.
    Replace,
}

impl Capability {
    /// Verb used in the human-readable verdict.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Overwrite => "overwritten",
            Self::Replace => "deleted, renamed, or taken over",
        }
    }
}

/// How the unsafe access is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantSource {
    /// An allowing ACE in the object's DACL.
    Dacl,
    /// The object's owner, who is always implicitly granted `WRITE_DAC`.
    Owner,
}

/// A single reason a location cannot host a persisted executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclViolation {
    /// Executable or directory that carries the grant.
    pub path: PathBuf,
    /// What the grant allows.
    pub capability: Capability,
    /// How the grant reaches the trustee.
    pub source: GrantSource,
    /// Rendered SID of the trustee, or a bracketed explanation when the ACE
    /// could not be interpreted.
    pub trustee_sid: String,
    /// Human-readable description of the trustee's relationship to this
    /// process ("Everyone", "the account this process runs as", …).
    pub trustee: String,
    /// The matched access, rendered for humans.
    pub access: String,
}

impl AclViolation {
    /// One sentence naming the path, what can happen to it, and who can do it.
    #[must_use]
    pub fn describe(&self) -> String {
        let trustee = format!("{} ({})", self.trustee, self.trustee_sid);
        match self.source {
            GrantSource::Dacl => format!(
                "{} can be {} by {trustee} [{}]",
                self.path.display(),
                self.capability.verb(),
                self.access
            ),
            GrantSource::Owner => format!(
                "{} is owned by {trustee}; an owner is always allowed to rewrite an object's \
                 permissions",
                self.path.display()
            ),
        }
    }
}

/// The verdict for one executable path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSafety {
    /// Resolved executable the verdict describes.
    pub target: PathBuf,
    /// Every path that was inspected, closest to the executable first.
    pub checked: Vec<PathBuf>,
    /// The first unsafe grant found, if any.
    pub violation: Option<AclViolation>,
    /// Diagnostics that did not change the verdict but qualify its strength.
    pub notes: Vec<String>,
}

impl PathSafety {
    /// True only when every inspected path was proven safe.
    #[must_use]
    pub const fn is_protected(&self) -> bool {
        self.violation.is_none()
    }

    /// Actionable verdict: what is wrong and what to do about it.
    #[must_use]
    pub fn summary(&self) -> String {
        self.violation.as_ref().map_or_else(
            || {
                format!(
                    "{} is a protected location for a persisted controller",
                    self.target.display()
                )
            },
            |violation| format!("{}; {}", violation.describe(), remedy()),
        )
    }
}

/// The directory `install.ps1` uses, and the one every refusal points at.
#[must_use]
pub fn suggested_install_directory() -> PathBuf {
    let root = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    root.join("Winter")
}

/// The remedy every refusal carries, so the message is actionable as printed.
fn remedy() -> String {
    format!(
        "install the binaries in a directory only administrators can write (for example {}) and \
         run this command from that copy",
        suggested_install_directory().display()
    )
}

/// Inspects `target` and every ancestor directory on the way to the volume root.
///
/// Returns a verdict rather than an error for an unsafe location; a real
/// failure (a descriptor that cannot be read at all) is an error, because the
/// caller must be able to tell "proven unsafe" from "could not be proven safe".
pub fn check_path_safety(target: &Path) -> PlatformResult<PathSafety> {
    let resolved = resolve(target);
    let (trustees, notes) = unprivileged_trustees()?;

    let mut checked = Vec::new();
    let mut violation = inspect(&resolved, &trustees, Capability::Overwrite)?;
    checked.push(resolved.clone());

    if violation.is_none() {
        for directory in ancestors(&resolved) {
            let found = inspect(&directory, &trustees, Capability::Replace)?;
            checked.push(directory);
            if let Some(found) = found {
                violation = Some(found);
                break;
            }
        }
    }

    Ok(PathSafety {
        target: resolved,
        checked,
        violation,
        notes,
    })
}

/// [`check_path_safety`], turned into the error a caller can return verbatim.
pub fn require_protected_path(target: &Path) -> PlatformResult<PathSafety> {
    let safety = check_path_safety(target)?;
    if safety.is_protected() {
        return Ok(safety);
    }
    Err(PlatformError::UnprotectedInstallPath(format!(
        "refusing to persist a logon task for {}: {}",
        target.display(),
        safety.summary()
    )))
}

/// Resolves `target` to the canonical long path that the ACLs actually live on.
///
/// Resolution matters twice: an 8.3 alias (`C:\Users\ADMINI~1\…`) and a
/// junction both mean the write check must run against the real directory, not
/// against the spelling that happens to be in `%TEMP%`. A path that cannot be
/// resolved is returned unchanged, so the descriptor lookup fails closed.
fn resolve(target: &Path) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(target) else {
        return target.to_path_buf();
    };
    let rendered = resolved.display().to_string();
    match rendered.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

/// Every directory above `target`, from its parent up to the volume root.
fn ancestors(target: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = target.parent().map(Path::to_path_buf);
    while let Some(directory) = current {
        current = directory.parent().map(Path::to_path_buf);
        directories.push(directory);
    }
    directories
}

/// Checks one path against the unsafe trustee set.
fn inspect(
    path: &Path,
    trustees: &[Trustee],
    capability: Capability,
) -> PlatformResult<Option<AclViolation>> {
    let descriptor = read_descriptor(path)?;

    if let Some(sid) = owner_sid(&descriptor)?
        && let Some(trustee) = trustees.iter().find(|trustee| trustee.sid == sid)
    {
        return Ok(Some(violation(
            path,
            capability,
            GrantSource::Owner,
            &sid,
            trustee.description,
            "owner".to_owned(),
        )));
    }

    let mask = effective_mask(capability);
    for grant in dacl_grants(&descriptor)? {
        if grant.refuse {
            return Ok(Some(violation(
                path,
                capability,
                GrantSource::Dacl,
                &grant.trustee_sid,
                "an access-control entry Winter cannot interpret, which is treated as unsafe",
                grant.access,
            )));
        }
        if grant.mask & mask == 0 {
            continue;
        }
        if let Some(trustee) = trustees
            .iter()
            .find(|trustee| trustee.sid == grant.trustee_sid)
        {
            return Ok(Some(violation(
                path,
                capability,
                GrantSource::Dacl,
                &grant.trustee_sid,
                trustee.description,
                render_access(grant.mask & mask),
            )));
        }
    }

    Ok(None)
}

fn violation(
    path: &Path,
    capability: Capability,
    source: GrantSource,
    trustee_sid: &str,
    trustee: &str,
    access: String,
) -> AclViolation {
    AclViolation {
        path: path.to_path_buf(),
        capability,
        source,
        trustee_sid: trustee_sid.to_owned(),
        trustee: trustee.to_owned(),
        access,
    }
}

/// The access bits that make a path unsafe for the given capability.
///
/// `FILE_ADD_FILE` and `FILE_ADD_SUBDIRECTORY` are the directory spellings of
/// `FILE_WRITE_DATA` and `FILE_APPEND_DATA`: the same bits, named both ways so
/// the mask reads correctly for files and for directories.
const fn effective_mask(capability: Capability) -> u32 {
    /// Modify an existing object: rewrite its data, delete it, or rewrite its
    /// permissions and owner.
    const OVERWRITE: u32 = FILE_WRITE_DATA.0
        | FILE_ADD_FILE.0
        | FILE_APPEND_DATA.0
        | FILE_ADD_SUBDIRECTORY.0
        | FILE_WRITE_EA.0
        | FILE_WRITE_ATTRIBUTES.0
        | DELETE.0
        | WRITE_DAC.0
        | WRITE_OWNER.0
        | GENERIC_WRITE.0
        | GENERIC_ALL.0;
    /// Remove or seize the directory that holds the next element of the path.
    const REPLACE: u32 =
        FILE_DELETE_CHILD.0 | DELETE.0 | WRITE_DAC.0 | WRITE_OWNER.0 | GENERIC_ALL.0;
    match capability {
        Capability::Overwrite => OVERWRITE,
        Capability::Replace => REPLACE,
    }
}

/// A principal a medium-integrity process of this account can act as.
#[derive(Debug)]
struct Trustee {
    sid: String,
    description: &'static str,
}

/// Well-known trustees whose write access any process of this account can use.
const UNPRIVILEGED_WELL_KNOWN: &[(WELL_KNOWN_SID_TYPE, &str)] = &[
    (WinWorldSid, "Everyone"),
    (WinAuthenticatedUserSid, "Authenticated Users"),
    (WinBuiltinUsersSid, r"BUILTIN\Users"),
    (WinInteractiveSid, "INTERACTIVE"),
    (WinConsoleLogonSid, "CONSOLE LOGON"),
    (WinAnonymousSid, "ANONYMOUS LOGON"),
    (WinBuiltinAnyPackageSid, "ALL APPLICATION PACKAGES"),
];

/// Trustees a medium-integrity process cannot use, so their write access is not
/// a reason to refuse a location.
const PRIVILEGED_WELL_KNOWN: &[WELL_KNOWN_SID_TYPE] = &[
    WinLocalSystemSid,
    WinBuiltinAdministratorsSid,
    WinLocalServiceSid,
    WinNetworkServiceSid,
];

/// `NT SERVICE\TrustedInstaller`, the owner of `%ProgramFiles%` and
/// `C:\Windows`; it has no well-known-SID constant in the `windows` crate.
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Builds the unsafe trustee set, plus diagnostics that weaken it.
///
/// The account's own SID is the decisive entry on a stock Windows install: the
/// default Cargo location `%USERPROFILE%\.cargo\bin` inherits full control for
/// the account only, and names neither Everyone, nor Authenticated Users, nor
/// `BUILTIN\Users`. A check limited to those well-known SIDs would therefore
/// have accepted the very path this module exists to refuse.
fn unprivileged_trustees() -> PlatformResult<(Vec<Trustee>, Vec<String>)> {
    let mut trustees = Vec::new();
    for (kind, description) in UNPRIVILEGED_WELL_KNOWN {
        if let Some(sid) = well_known_sid_string(*kind) {
            trustees.push(Trustee { sid, description });
        } else {
            return Err(PlatformError::SecurityDescriptorQuery(format!(
                "Windows did not produce the well-known SID for {description}"
            )));
        }
    }

    let account = current_user_sid_string().map_err(|error| {
        PlatformError::SecurityDescriptorQuery(format!(
            "the account SID is required to prove the controller's location: {error}"
        ))
    })?;
    trustees.push(Trustee {
        sid: account,
        description: "the account this process runs as",
    });

    let mut notes = Vec::new();
    let privileged = PRIVILEGED_WELL_KNOWN
        .iter()
        .filter_map(|kind| well_known_sid_string(*kind))
        .chain(std::iter::once(TRUSTED_INSTALLER_SID.to_owned()))
        .collect::<Vec<_>>();

    match enabled_token_group_sids() {
        Ok(groups) => {
            for sid in groups {
                if privileged.contains(&sid) || trustees.iter().any(|t| t.sid == sid) {
                    continue;
                }
                trustees.push(Trustee {
                    sid,
                    description: "a group this account's token holds enabled",
                });
            }
        }
        // Falling back to the well-known set plus the account SID is a real
        // reduction in coverage, so it is reported instead of hidden.
        Err(error) => notes.push(format!(
            "the token's group list could not be read ({error}), so only the well-known \
             trustees and the account SID were checked"
        )),
    }

    Ok((trustees, notes))
}

/// Renders a well-known SID through the OS instead of hard-coding its string.
fn well_known_sid_string(kind: WELL_KNOWN_SID_TYPE) -> Option<String> {
    let mut buffer = [0_u8; MAX_SID_SIZE];
    let mut size = MAX_SID_SIZE as u32;
    // SAFETY: `buffer` is writable for `size` bytes — the documented maximum
    // SID size — and `size` is a writable out parameter.
    unsafe {
        CreateWellKnownSid(
            kind,
            None,
            Some(PSID(buffer.as_mut_ptr().cast())),
            &mut size,
        )
    }
    .ok()?;
    sid_to_string(&buffer[..size as usize])
}

/// String SIDs of the groups this token can actually use.
///
/// Groups the token holds only to deny (`SE_GROUP_USE_FOR_DENY_ONLY` — where an
/// unelevated administrator token keeps `BUILTIN\Administrators`) and entries
/// that are not `SE_GROUP_ENABLED` (integrity levels, the logon-session SID)
/// cannot open anything and are left out.
fn enabled_token_group_sids() -> PlatformResult<Vec<String>> {
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
    let _ = unsafe { GetTokenInformation(*token, TokenGroups, None, 0, &mut needed) };
    if needed == 0 {
        return Ok(Vec::new());
    }

    let mut buffer = vec![0_u8; needed as usize];
    // SAFETY: `buffer` is exactly `needed` bytes and stays alive and uniquely
    // borrowed for the duration of the call.
    unsafe {
        GetTokenInformation(
            *token,
            TokenGroups,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|source| PlatformError::win32("GetTokenInformation(TokenGroups)", source))?;

    Ok(parse_token_groups(&buffer)
        .into_iter()
        .filter(|(_, attributes)| {
            attributes & SE_GROUP_ENABLED as u32 != 0
                && attributes & SE_GROUP_USE_FOR_DENY_ONLY as u32 == 0
        })
        .map(|(sid, _)| sid)
        .collect())
}

/// Reads `(SID, attributes)` pairs out of a `TOKEN_GROUPS` image.
///
/// Split out of the Win32 call so the layout arithmetic can be unit tested on a
/// synthetic buffer; every bounds check that fails simply drops the entry, and
/// the caller then works with a smaller (never a larger) trustee set.
fn parse_token_groups(buffer: &[u8]) -> Vec<(String, u32)> {
    let mut groups = Vec::new();
    let groups_offset = offset_of!(TOKEN_GROUPS, Groups);
    if buffer.len() < groups_offset + size_of::<u32>() {
        return groups;
    }
    // SAFETY: the buffer is at least large enough for the group count, which is
    // the first field of TOKEN_GROUPS.
    let count = unsafe { read_unaligned(buffer.as_ptr().cast::<u32>()) } as usize;
    let stride = size_of::<SID_AND_ATTRIBUTES>();
    let attributes_offset = offset_of!(SID_AND_ATTRIBUTES, Attributes);

    for index in 0..count {
        let offset = groups_offset + index * stride;
        if offset + stride > buffer.len() {
            break;
        }
        // SAFETY: `offset + stride` was just proven to be inside `buffer`, so
        // both the pointer field and the attribute field are readable.
        let (sid, attributes) = unsafe {
            (
                read_unaligned(buffer.as_ptr().add(offset).cast::<*const u8>()),
                read_unaligned(
                    buffer
                        .as_ptr()
                        .add(offset + attributes_offset)
                        .cast::<u32>(),
                ),
            )
        };
        if sid.is_null() {
            continue;
        }
        let start = (sid as usize).wrapping_sub(buffer.as_ptr() as usize);
        if start < offset + stride || start + 8 > buffer.len() {
            continue;
        }
        let length = 8 + usize::from(buffer[start + 1]) * size_of::<u32>();
        if start + length > buffer.len() {
            continue;
        }
        if let Some(rendered) = sid_to_string(&buffer[start..start + length]) {
            groups.push((rendered, attributes));
        }
    }

    groups
}

/// Owns the security descriptor returned by `GetFileSecurityW`.
struct Descriptor {
    buffer: Vec<u8>,
}

impl Descriptor {
    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.buffer.as_ptr().cast_mut().cast())
    }
}

/// Reads the owner and DACL of `path`.
fn read_descriptor(path: &Path) -> PlatformResult<Descriptor> {
    use std::os::windows::ffi::OsStrExt;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let information = DACL_SECURITY_INFORMATION.0 | OWNER_SECURITY_INFORMATION.0;

    let mut needed = 0_u32;
    // SAFETY: the sizing call passes a null descriptor with length 0, which is
    // the documented way to ask for the required buffer size; nothing is
    // dereferenced and `needed` is a writable out parameter.
    let _ = unsafe { GetFileSecurityW(PCWSTR(wide.as_ptr()), information, None, 0, &mut needed) };
    if needed == 0 {
        return Err(PlatformError::SecurityDescriptorQuery(format!(
            "Windows reported no security descriptor size for {}",
            path.display()
        )));
    }

    let mut buffer = vec![0_u8; needed as usize];
    // SAFETY: `buffer` is `needed` bytes long, which is exactly what the
    // sizing call asked for, and it stays alive for the whole call.
    let result = unsafe {
        GetFileSecurityW(
            PCWSTR(wide.as_ptr()),
            information,
            Some(PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast())),
            needed,
            &mut needed,
        )
    };
    if !result.as_bool() {
        return Err(PlatformError::SecurityDescriptorRead {
            path: path.to_path_buf(),
            source: windows::core::Error::from_thread(),
        });
    }

    Ok(Descriptor { buffer })
}

/// Renders the descriptor's owner as a string SID.
fn owner_sid(descriptor: &Descriptor) -> PlatformResult<Option<String>> {
    let mut owner = PSID(std::ptr::null_mut());
    let mut defaulted = BOOL(0);
    // SAFETY: `descriptor` owns the buffer the descriptor pointer refers to,
    // and both out parameters are writable. The returned SID points into that
    // buffer, which is borrowed for this call.
    unsafe { GetSecurityDescriptorOwner(descriptor.as_ptr(), &mut owner, &mut defaulted) }
        .map_err(|source| PlatformError::win32("GetSecurityDescriptorOwner", source))?;
    if owner.0.is_null() {
        return Ok(None);
    }

    // SAFETY: the owner points into the descriptor buffer validated above and
    // keeps that buffer alive for this call; a SID starts with eight readable
    // header bytes.
    let header = unsafe { std::slice::from_raw_parts(owner.0.cast::<u8>(), 8) };
    let length = 8 + usize::from(header[1]) * size_of::<u32>();
    // SAFETY: same allocation, and `length` was derived from the SID's own
    // sub-authority count.
    let bytes = unsafe { std::slice::from_raw_parts(owner.0.cast::<u8>(), length) };
    Ok(sid_to_string(bytes))
}

/// One allowing ACE, reduced to owned data.
#[derive(Debug)]
struct Grant {
    mask: u32,
    trustee_sid: String,
    access: String,
    /// True when the grant is unsafe by itself: the ACE could not be read, the
    /// DACL is absent, or the ACE type is one this code does not understand.
    refuse: bool,
}

/// Collects the ACEs that can grant access to the object itself.
///
/// Inherit-only ACEs are skipped: they describe what children receive and grant
/// nothing here. That is not a detail — `%ProgramFiles%` carries
/// `CREATOR OWNER:(OI)(CI)(IO)(F)`, and honouring it would refuse the
/// directory every install is supposed to use.
fn dacl_grants(descriptor: &Descriptor) -> PlatformResult<Vec<Grant>> {
    let mut present = BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = BOOL(0);
    // SAFETY: `descriptor` owns the buffer the pointer refers to, and all three
    // out parameters are writable.
    unsafe {
        GetSecurityDescriptorDacl(descriptor.as_ptr(), &mut present, &mut dacl, &mut defaulted)
    }
    .map_err(|source| PlatformError::win32("GetSecurityDescriptorDacl", source))?;

    if !present.as_bool() || dacl.is_null() {
        // A missing DACL is not "no access": it grants every account full
        // control, so it is a refusal, not a pass.
        return Ok(vec![Grant {
            mask: u32::MAX,
            trustee_sid: "S-1-1-0".to_owned(),
            access: "an absent DACL, which grants every account full control".to_owned(),
            refuse: true,
        }]);
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `size` is exactly the structure the class expects, and `dacl` is
    // the valid ACL returned above.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(|source| PlatformError::win32("GetAclInformation", source))?;

    let base = dacl.cast::<u8>();
    let used = size.AclBytesInUse as usize;
    let mut grants = Vec::with_capacity(size.AceCount as usize);

    for index in 0..size.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `index` is below the ACE count reported for this ACL, and
        // `ace` is a writable out parameter.
        if let Err(source) = unsafe { GetAce(dacl, index, &mut ace) } {
            return Err(PlatformError::win32("GetAce", source));
        }
        if ace.is_null() {
            return Err(PlatformError::SecurityDescriptorQuery(format!(
                "Windows returned no ACE for index {index}"
            )));
        }

        let address = ace.cast::<u8>();
        let offset = (address as usize).wrapping_sub(base as usize);
        // Every ACE starts with ACE_HEADER { AceType, AceFlags, AceSize } and
        // carries its access mask directly after it, for every ACE type
        // Windows defines. Reading those eight bytes is therefore safe for any
        // type; everything past them depends on the type.
        if offset > used || used - offset < 8 {
            grants.push(unreadable_grant(format!(
                "an ACE at byte {offset} of a {used}-byte DACL"
            )));
            continue;
        }
        // SAFETY: the eight bytes at `offset` were just proven to be inside the
        // ACL image, so the header and mask are readable.
        let (ace_type, ace_flags, ace_size, mask) = unsafe {
            (
                *address,
                *address.add(1),
                u16::from_le_bytes([*address.add(2), *address.add(3)]),
                read_unaligned(address.add(4).cast::<u32>()),
            )
        };

        if ace_flags & INHERIT_ONLY_ACE.0 as u8 != 0 {
            continue;
        }
        if is_non_granting(ace_type) {
            // Denies, audits, and the mandatory label do not grant anything.
            // The label is why the type matters at all:
            // `SYSTEM_MANDATORY_LABEL_NO_READ_UP` is 0x2, which read as an
            // access mask would look exactly like `FILE_WRITE_DATA`.
            continue;
        }
        if ace_size as usize >= 8 && offset + ace_size as usize > used {
            grants.push(unreadable_grant(format!(
                "an ACE at byte {offset} that claims {ace_size} bytes of a {used}-byte DACL"
            )));
            continue;
        }
        let Some(sid) = ace_sid(address, ace_size as usize, ace_type) else {
            grants.push(unreadable_grant(format!(
                "an ACE of type {ace_type} at byte {offset} with an unreadable trustee"
            )));
            continue;
        };

        if !is_granting(ace_type) {
            // An ACE type this code does not know. Refusing is the only honest
            // reading: the bits might grant write access.
            grants.push(unreadable_grant(format!(
                "unsupported ACE type {ace_type} for trustee {sid}"
            )));
            continue;
        }

        grants.push(Grant {
            mask,
            trustee_sid: sid,
            access: render_access(mask),
            refuse: false,
        });
    }

    Ok(grants)
}

/// An ACE that could not be interpreted, which is always a refusal.
fn unreadable_grant(reason: String) -> Grant {
    Grant {
        mask: u32::MAX,
        trustee_sid: format!("<unreadable: {reason}>"),
        access: reason,
        refuse: true,
    }
}

/// True for ACE types that never grant access to the object.
fn is_non_granting(ace_type: u8) -> bool {
    [
        ACCESS_DENIED_ACE_TYPE,
        ACCESS_DENIED_OBJECT_ACE_TYPE,
        ACCESS_DENIED_CALLBACK_ACE_TYPE,
        ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
        SYSTEM_AUDIT_ACE_TYPE,
        SYSTEM_AUDIT_OBJECT_ACE_TYPE,
        SYSTEM_AUDIT_CALLBACK_ACE_TYPE,
        SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE,
        SYSTEM_ALARM_ACE_TYPE,
        SYSTEM_ALARM_OBJECT_ACE_TYPE,
        SYSTEM_ALARM_CALLBACK_ACE_TYPE,
        SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE,
        SYSTEM_MANDATORY_LABEL_ACE_TYPE,
        SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE,
        SYSTEM_SCOPED_POLICY_ID_ACE_TYPE,
        SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE,
        SYSTEM_ACCESS_FILTER_ACE_TYPE,
    ]
    .iter()
    .any(|kind| *kind as u8 == ace_type)
}

/// True for the ACE types whose `Mask` grants access.
fn is_granting(ace_type: u8) -> bool {
    [
        ACCESS_ALLOWED_ACE_TYPE,
        ACCESS_ALLOWED_OBJECT_ACE_TYPE,
        ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
        ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE,
    ]
    .iter()
    .any(|kind| *kind as u8 == ace_type)
}

/// Extracts the trustee SID of a granting ACE.
///
/// The four granting types share the header and the mask but not the offset of
/// the SID: the object forms insert the object and inherited-object GUIDs the
/// `Flags` field announces. The result is bounded by `ace_size` and by the
/// SID's own length, so a malformed ACE yields `None` rather than a read past
/// the ACL.
fn ace_sid(address: *const u8, ace_size: usize, ace_type: u8) -> Option<String> {
    if ace_size < 8 {
        return None;
    }
    let object_form = ace_type as u32 == ACCESS_ALLOWED_OBJECT_ACE_TYPE
        || ace_type as u32 == ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE;
    let mut sid_offset = 8;
    if object_form {
        // SAFETY: `ace_size >= 8` was checked by the caller and the ACE-byte
        // bounds were validated against the ACL image before this call.
        let flags = unsafe { read_unaligned(address.add(8).cast::<u32>()) };
        sid_offset += 4;
        if flags & ACE_OBJECT_TYPE_PRESENT.0 != 0 {
            sid_offset += 16;
        }
        if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT.0 != 0 {
            sid_offset += 16;
        }
    }
    if sid_offset + 8 > ace_size {
        return None;
    }

    // SAFETY: `sid_offset + 8` is inside the ACE, so the SID header is readable.
    let length = {
        let count = unsafe { *address.add(sid_offset + 1) };
        8 + usize::from(count) * size_of::<u32>()
    };
    if sid_offset + length > ace_size {
        return None;
    }
    // SAFETY: the whole SID was just proven to lie inside the ACE.
    let bytes = unsafe { std::slice::from_raw_parts(address.add(sid_offset), length) };
    sid_to_string(bytes)
}

/// Renders an access mask as the names a user sees in `icacls`-style output.
fn render_access(mask: u32) -> String {
    const NAMES: &[(u32, &str)] = &[
        (FILE_WRITE_DATA.0, "write data/add file"),
        (FILE_APPEND_DATA.0, "append data/add subdirectory"),
        (FILE_WRITE_EA.0, "write extended attributes"),
        (FILE_WRITE_ATTRIBUTES.0, "write attributes"),
        (FILE_DELETE_CHILD.0, "delete child"),
        (DELETE.0, "delete"),
        (WRITE_DAC.0, "change permissions"),
        (WRITE_OWNER.0, "take ownership"),
        (GENERIC_WRITE.0, "generic write"),
        (GENERIC_ALL.0, "generic all"),
    ];
    let mut rendered = Vec::new();
    for (bit, name) in NAMES.iter().copied() {
        if mask & bit != 0 && !rendered.contains(&name) {
            rendered.push(name);
        }
    }
    if rendered.is_empty() {
        format!("access mask {mask:#x}")
    } else {
        rendered.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_account_sid() -> String {
        current_user_sid_string().expect("the token must expose a user SID")
    }

    /// The gate has to accept the directories a machine-level install uses and
    /// refuse the ones a normal account can write. The first three rows are
    /// live Windows directories, so this is also the test that proves the
    /// module reads real ACLs rather than a fixture.
    #[test]
    fn machine_directories_are_protected_and_user_directories_are_not() {
        let temp = env::temp_dir();
        let cargo_bin =
            env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo").join("bin"));

        let mut cases: Vec<(PathBuf, bool, &str)> = vec![
            (
                PathBuf::from(r"C:\Windows\System32"),
                true,
                "the OS directory is writable only by administrators",
            ),
            (
                suggested_install_directory()
                    .parent()
                    .expect("the suggestion has a parent")
                    .to_path_buf(),
                true,
                "the machine program directory is writable only by administrators",
            ),
            (
                temp.clone(),
                false,
                "the process temporary directory belongs to the account",
            ),
        ];
        if let Some(cargo_bin) = cargo_bin.filter(|path| path.is_dir()) {
            cases.push((
                cargo_bin,
                false,
                "the default Cargo install location belongs to the account",
            ));
        }

        for (path, expected, why) in cases {
            let safety = check_path_safety(&path)
                .unwrap_or_else(|error| panic!("{} must be inspectable: {error}", path.display()));
            assert_eq!(
                safety.is_protected(),
                expected,
                "{}: {why}; verdict {safety:?}",
                path.display()
            );
        }
    }

    /// Live probe: prints the verdict for every location an install can end up
    /// in, and fails when a verdict contradicts what the location means.
    ///
    /// Ignored by default because the verdicts are properties of the machine,
    /// not of the code. Run it on a real host:
    ///
    /// ```text
    /// cargo test --locked --lib acl::tests::live_path_verdicts -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "reports machine-specific ACL verdicts; run it explicitly"]
    fn live_path_verdicts() {
        let home = env::var_os("USERPROFILE").map(PathBuf::from);
        let mut cases: Vec<(PathBuf, Option<bool>, &str)> = vec![
            (
                PathBuf::from(r"C:\Windows\System32"),
                Some(true),
                "the OS directory is machine-protected",
            ),
            (
                PathBuf::from(r"C:\Program Files"),
                Some(true),
                "the machine program directory is machine-protected",
            ),
            (
                suggested_install_directory().join("winterd.exe"),
                Some(true),
                "the machine directory every install.ps1 deployment must pass",
            ),
            (
                suggested_install_directory(),
                Some(true),
                "the machine directory every install.ps1 deployment must pass",
            ),
            (
                env::temp_dir(),
                Some(false),
                "the process temporary directory is account-writable",
            ),
        ];
        if let Some(home) = &home {
            cases.insert(
                0,
                (
                    home.join(".cargo").join("bin"),
                    Some(false),
                    "the default Cargo install location is account-writable",
                ),
            );
        }

        for (path, expected, why) in cases {
            if !path.exists() {
                println!("(skipped, does not exist) {} — {why}", path.display());
                continue;
            }
            let safety = check_path_safety(&path)
                .unwrap_or_else(|error| panic!("{} must be inspectable: {error}", path.display()));
            let verdict = if safety.is_protected() {
                "PROTECTED"
            } else {
                "UNPROTECTED"
            };
            println!("{verdict:11} {} — {why}", path.display());
            println!("            checked: {:?}", safety.checked);
            if let Some(violation) = &safety.violation {
                println!("            reason : {}", violation.describe());
            }
            for note in &safety.notes {
                println!("            note   : {note}");
            }
            if let Some(expected) = expected {
                assert_eq!(
                    safety.is_protected(),
                    expected,
                    "{}: {why}; verdict {safety:?}",
                    path.display()
                );
            }
        }
    }

    /// The account's own SID is the only reason `%USERPROFILE%\.cargo\bin` is
    /// refused, so the trustee set must contain it even when no well-known
    /// group is named by any ACE.
    #[test]
    fn the_account_sid_is_always_an_unsafe_trustee() {
        let (trustees, _) = unprivileged_trustees().expect("the trustee set must be buildable");
        let account = current_account_sid();

        assert!(
            trustees.iter().any(|trustee| trustee.sid == account),
            "the account SID must be in the unsafe set: {trustees:?}"
        );
        for name in [
            "Everyone",
            "Authenticated Users",
            r"BUILTIN\Users",
            "INTERACTIVE",
        ] {
            assert!(
                trustees.iter().any(|trustee| trustee.description == name),
                "{name} must be in the unsafe set: {trustees:?}"
            );
        }
        // The trustee a `%ProgramFiles%` install must survive.
        assert!(
            !trustees.iter().any(|trustee| trustee.sid == "S-1-5-32-544"),
            "BUILTIN\\Administrators must not be an unsafe trustee: {trustees:?}"
        );
    }

    /// A directory that inherits write access from its parent is refused, and
    /// the verdict names the *ancestor*, not the directory under test: that is
    /// the actionable half of the message.
    #[test]
    fn an_ancestor_that_the_account_owns_refuses_a_leaf_directory() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let leaf = temp.path().join("winter-leaf");
        std::fs::create_dir(&leaf).expect("the fixture directory should be created");

        let safety = check_path_safety(&leaf).expect("the fixture must be inspectable");
        let violation = safety
            .violation
            .as_ref()
            .expect("a directory below a user profile must be refused");
        assert!(
            safety.checked.contains(&violation.path),
            "the verdict must name an inspected path: {safety:?}"
        );
        assert!(
            !safety.checked.is_empty() && safety.checked[0] == resolve(&leaf),
            "the executable's own directory is inspected first: {safety:?}"
        );
        assert!(
            violation.trustee_sid.starts_with("S-1-"),
            "the verdict must name the trustee by SID: {violation:?}"
        );
        assert!(
            safety.summary().contains("administrators can write"),
            "the verdict must carry the remedy: {}",
            safety.summary()
        );
    }

    /// Every ancestor up to the volume root is inspected, in order.
    #[test]
    fn the_walk_reaches_the_volume_root() {
        let directories = ancestors(Path::new(r"C:\Winter\bin\winterd.exe"));

        assert_eq!(
            directories,
            vec![
                PathBuf::from(r"C:\Winter\bin"),
                PathBuf::from(r"C:\Winter"),
                PathBuf::from(r"C:\"),
            ]
        );
    }

    /// The mandatory label is not an access mask: `NO_READ_UP` is the same
    /// number as `FILE_WRITE_DATA`, so reading the bits without checking the
    /// ACE type would refuse every protected directory on the machine.
    #[test]
    fn the_mandatory_label_is_never_read_as_write_access() {
        assert!(is_non_granting(SYSTEM_MANDATORY_LABEL_ACE_TYPE as u8));
        assert!(!is_granting(SYSTEM_MANDATORY_LABEL_ACE_TYPE as u8));
        assert_eq!(
            SYSTEM_MANDATORY_LABEL_NO_READ_UP_BIT & FILE_WRITE_DATA.0,
            FILE_WRITE_DATA.0,
            "the bit collision this test guards against must still exist"
        );
    }

    const SYSTEM_MANDATORY_LABEL_NO_READ_UP_BIT: u32 = 2;

    /// A deny ACE, an inherit-only ACE, and an allow ACE in one ACL: only the
    /// last grants access to the object itself.
    #[test]
    fn only_object_applicable_allow_aces_grant_access() {
        let account = current_account_sid();
        let sid = sid_bytes(&account);

        let grants = vec![
            deny_ace(&sid, FILE_WRITE_DATA.0),
            allow_ace(&sid, DELETE.0, INHERIT_ONLY_ACE.0 as u8),
            allow_ace(&sid, DELETE.0, 0),
        ];
        let dacl = acl_image(&grants);

        let descriptor = Descriptor { buffer: dacl };
        let parsed = dacl_grants(&descriptor).expect("the synthetic DACL must parse");

        assert_eq!(parsed.len(), 1, "only one ACE applies here: {parsed:?}");
        assert_eq!(parsed[0].trustee_sid, account);
        assert_eq!(parsed[0].mask, DELETE.0);
        assert!(!parsed[0].refuse);
    }

    /// An ACE type Windows adds later must refuse the location rather than be
    /// silently skipped.
    #[test]
    fn an_unknown_ace_type_refuses_the_location() {
        let sid = sid_bytes(&current_account_sid());
        let unknown = allow_ace(&sid, FILE_WRITE_DATA.0, 0);
        let mut raw = unknown.clone();
        raw[0] = 0x7f;

        let descriptor = Descriptor {
            buffer: acl_image(&[raw]),
        };
        let parsed = dacl_grants(&descriptor).expect("the synthetic DACL must parse");

        assert_eq!(parsed.len(), 1);
        assert!(
            parsed[0].refuse,
            "an unknown ACE type must refuse: {parsed:?}"
        );
    }

    /// A minimal self-relative SECURITY_DESCRIPTOR with no DACL present.
    #[test]
    fn an_absent_dacl_refuses_the_location() {
        // Control word: SE_SELF_RELATIVE only, so SE_DACL_PRESENT is clear.
        let mut buffer = vec![0_u8; 20];
        buffer[0] = 1; // revision
        buffer[3] = 0x80; // SE_SELF_RELATIVE >> 8
        let descriptor = Descriptor { buffer };

        let grants = dacl_grants(&descriptor).expect("a descriptor without a DACL must parse");

        assert_eq!(grants.len(), 1);
        assert!(
            grants[0].refuse,
            "an absent DACL grants everything: {grants:?}"
        );
    }

    /// The token-group parse must honour the enabled/deny-only distinction and
    /// stay inside its buffer.
    #[test]
    fn token_groups_are_parsed_with_their_attributes() {
        let enabled = sid_bytes("S-1-5-32-545");
        let deny_only = sid_bytes("S-1-5-32-544");
        let buffer = token_groups_image(&[
            (&enabled, SE_GROUP_ENABLED as u32),
            (&deny_only, SE_GROUP_USE_FOR_DENY_ONLY as u32),
        ]);

        let parsed = parse_token_groups(&buffer);

        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert_eq!(parsed[0].0, "S-1-5-32-545");
        assert_eq!(parsed[1].0, "S-1-5-32-544");
        assert_eq!(parsed[1].1, SE_GROUP_USE_FOR_DENY_ONLY as u32);
    }

    /// A truncated buffer yields fewer entries instead of reading past its end.
    #[test]
    fn a_truncated_token_groups_buffer_is_tolerated() {
        let sid = sid_bytes("S-1-5-32-545");
        let full = token_groups_image(&[(&sid, SE_GROUP_ENABLED as u32)]);

        for length in 0..full.len() {
            let truncated = &full[..length];
            let parsed = parse_token_groups(truncated);
            assert!(
                parsed.is_empty(),
                "a {length}-byte prefix must not yield groups: {parsed:?}"
            );
        }
    }

    /// The object forms of the allow ACEs carry the trustee after two optional
    /// GUIDs; the SID offset must follow the flags.
    #[test]
    fn object_ace_trustees_are_located_through_their_guids() {
        let account = current_account_sid();
        let sid = sid_bytes(&account);

        for (flags, extra) in [(0_u32, 0_usize), (1, 16), (2, 16), (3, 32)] {
            let mut ace = Vec::new();
            ace.extend_from_slice(&[ACCESS_ALLOWED_OBJECT_ACE_TYPE as u8, 0, 0, 0]);
            ace.extend_from_slice(&FILE_WRITE_DATA.0.to_le_bytes());
            ace.extend_from_slice(&flags.to_le_bytes());
            ace.extend_from_slice(&vec![0_u8; extra]);
            ace.extend_from_slice(&sid);
            let size = ace.len() as u16;
            ace[2..4].copy_from_slice(&size.to_le_bytes());

            let rendered = ace_sid(
                ace.as_ptr(),
                ace.len(),
                ACCESS_ALLOWED_OBJECT_ACE_TYPE as u8,
            );

            assert_eq!(
                rendered.as_deref(),
                Some(account.as_str()),
                "flags {flags:#x} must locate the trustee"
            );
        }
    }

    #[test]
    fn access_masks_render_the_matched_bits() {
        let rendered = render_access(FILE_WRITE_DATA.0 | DELETE.0);

        assert!(rendered.contains("write data/add file"), "{rendered}");
        assert!(rendered.contains("delete"), "{rendered}");
        assert_eq!(
            render_access(0),
            "access mask 0x0",
            "an empty mask must still render"
        );
    }

    #[test]
    fn the_remedy_points_at_the_machine_program_directory() {
        let remedy = remedy();

        assert!(remedy.contains("administrators can write"), "{remedy}");
        assert!(
            remedy.contains(&suggested_install_directory().display().to_string()),
            "{remedy}"
        );
    }

    /// Builds a SID image from its canonical string, mirroring what the token
    /// and the ACLs carry.
    fn sid_bytes(sid: &str) -> Vec<u8> {
        let mut parts = sid.split('-');
        assert_eq!(parts.next(), Some("S"));
        let revision = parts.next().expect("revision").parse::<u8>().unwrap();
        let authority = parts.next().expect("authority").parse::<u64>().unwrap();
        let sub_authorities = parts
            .map(|part| part.parse::<u32>().unwrap())
            .collect::<Vec<_>>();

        let mut bytes = vec![revision, sub_authorities.len() as u8];
        // The identifier authority is six bytes, big-endian.
        bytes.extend_from_slice(&authority.to_be_bytes()[2..8]);
        for sub in sub_authorities {
            bytes.extend_from_slice(&sub.to_le_bytes());
        }
        bytes
    }

    fn allow_ace(sid: &[u8], mask: u32, flags: u8) -> Vec<u8> {
        ace(ACCESS_ALLOWED_ACE_TYPE as u8, sid, mask, flags)
    }

    fn deny_ace(sid: &[u8], mask: u32) -> Vec<u8> {
        ace(ACCESS_DENIED_ACE_TYPE as u8, sid, mask, 0)
    }

    fn ace(kind: u8, sid: &[u8], mask: u32, flags: u8) -> Vec<u8> {
        let size = 8 + sid.len();
        let mut bytes = vec![kind, flags, 0, 0];
        bytes.extend_from_slice(&mask.to_le_bytes());
        bytes.extend_from_slice(sid);
        bytes[2..4].copy_from_slice(&(size as u16).to_le_bytes());
        bytes
    }

    /// Wraps a self-relative security descriptor around a DACL image.
    fn acl_image(aces: &[Vec<u8>]) -> Vec<u8> {
        let acl_size = 8 + aces.iter().map(Vec::len).sum::<usize>();
        let mut acl = vec![2_u8, 0];
        acl.extend_from_slice(&(acl_size as u16).to_le_bytes());
        acl.extend_from_slice(&(aces.len() as u16).to_le_bytes());
        acl.extend_from_slice(&0_u16.to_le_bytes());
        for ace in aces {
            acl.extend_from_slice(ace);
        }

        // SECURITY_DESCRIPTOR: revision, sbz1, control, offsets, then the ACL.
        // Control = SE_SELF_RELATIVE | SE_DACL_PRESENT = 0x8004.
        let acl_offset = 20_u32;
        let mut descriptor = vec![1_u8, 0, 0x04, 0x80];
        descriptor.extend_from_slice(&0_u32.to_le_bytes()); // owner
        descriptor.extend_from_slice(&0_u32.to_le_bytes()); // group
        descriptor.extend_from_slice(&0_u32.to_le_bytes()); // sacl
        descriptor.extend_from_slice(&acl_offset.to_le_bytes());
        assert_eq!(descriptor.len(), acl_offset as usize);
        descriptor.extend_from_slice(&acl);
        descriptor
    }

    /// Lays out a `TOKEN_GROUPS` image with the SIDs stored after the array, as
    /// the API returns it. The buffer is sized up front: the pointers written
    /// into it are absolute, so it must not move while it is being filled.
    fn token_groups_image(groups: &[(&[u8], u32)]) -> Vec<u8> {
        let stride = size_of::<SID_AND_ATTRIBUTES>();
        let groups_offset = offset_of!(TOKEN_GROUPS, Groups);
        let sids_offset = groups_offset + groups.len() * stride;
        let total = sids_offset + groups.iter().map(|(sid, _)| sid.len()).sum::<usize>();
        let mut buffer = vec![0_u8; total];
        buffer[..4].copy_from_slice(&(groups.len() as u32).to_le_bytes());
        let base = buffer.as_ptr() as usize;
        let mut cursor = sids_offset;
        for (index, (sid, attributes)) in groups.iter().enumerate() {
            let offset = groups_offset + index * stride;
            buffer[cursor..cursor + sid.len()].copy_from_slice(sid);
            buffer[offset..offset + 8].copy_from_slice(&((base + cursor) as u64).to_le_bytes());
            let attributes_offset = offset + offset_of!(SID_AND_ATTRIBUTES, Attributes);
            buffer[attributes_offset..attributes_offset + 4]
                .copy_from_slice(&attributes.to_le_bytes());
            cursor += sid.len();
        }
        buffer
    }
}
