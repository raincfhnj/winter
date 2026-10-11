//! Persistent autostart for the controller.
//!
//! Installing Winter is meant to be a one-time action: after
//! `winter install` the prefix works in every session, and it must keep
//! working after a reboot without the user remembering to run anything. This
//! module owns that promise by keeping one per-user **logon scheduled task**
//! alive:
//!
//! * the trigger is the current account's logon, so a restart is enough;
//! * the principal runs at the highest available run level, so the elevated
//!   controller starts silently instead of asking for UAC at every logon;
//! * `winter install` registers it, `winter uninstall` removes it, and
//!   `winter autostart` exposes the three operations directly.
//!
//! Registration needs administrator approval exactly once. When the caller is
//! not elevated this module asks for it through UAC and waits for the helper,
//! so `winter install` still returns a truthful report; a dismissed prompt is
//! reported as [`AutostartStatus::Skipped`] rather than an error, because the
//! Windows Terminal integration itself installed fine.
//!
//! Approving registration is the *only* approval the persisted task ever needs:
//! from then on Task Scheduler starts the controller elevated at every sign-in.
//! The executable therefore has to be one a medium-integrity process of this
//! account cannot replace, so [`enable`] refuses to register anything outside a
//! protected directory. There is deliberately no switch to skip that check: a
//! persisted elevated task pointing at a writable path is a privilege
//! escalation primitive, not a preference.

use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::windows::{
    PathSafety, PlatformError, TaskSpec, check_path_safety, current_user_sid_string, delete_task,
    is_current_process_elevated, query_task_xml, register_task, require_protected_path,
    run_current_process_elevated_and_wait, task_xml,
};
use crate::{AppError, AppResult};

/// Name of the managed task in the Task Scheduler root folder.
pub const TASK_NAME: &str = "WinterController";

/// Ownership marker stored in the task description.
///
/// A task with our name that does not carry this marker belongs to someone
/// else; Winter refuses to overwrite or delete it.
const OWNERSHIP_MARKER: &str = "Winter controller autostart";

const TASK_DESCRIPTION: &str = "Winter controller autostart (managed by `winter install`; remove with `winter autostart disable`)";

/// Delay after logon, so the task does not race the shell's own startup.
const LOGON_DELAY: &str = "PT5S";

/// The hidden switch used by the elevated helper process, in clap's spelling
/// (no leading dashes, because clap adds them).
pub const VIA_ELEVATION_LONG: &str = "via-elevation";

/// The same switch as it appears on the command line handed to the helper.
pub const VIA_ELEVATION_ARG: &str = "--via-elevation";

/// What happened to the managed task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutostartStatus {
    /// The task is registered, matches this binary, and is managed by Winter.
    Registered,
    /// The task already matched; nothing was written.
    Unchanged,
    /// A managed task existed but pointed somewhere else and was replaced.
    Updated,
    /// The managed task was deleted by this call.
    Removed,
    /// No task with the managed name exists.
    Missing,
    /// A task with the managed name exists but was not created by Winter.
    Foreign,
    /// Winter's own task exists, but it no longer starts this binary — it
    /// points at a moved, renamed, or deleted executable.
    Stale,
    /// The task could not be touched (for example a dismissed UAC prompt).
    Skipped,
}

/// Machine-readable autostart state, embedded in every report that can change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutostartReport {
    pub task_name: String,
    pub status: AutostartStatus,
    /// True when a Winter-managed task is registered and points at this binary.
    pub registered: bool,
    /// Executable the registered task starts, when it could be read.
    pub command: Option<String>,
    /// Run level of the registered task, when it could be read.
    pub run_level: Option<String>,
    /// Trigger kind of the registered task, when it could be read.
    pub trigger: Option<String>,
    /// True when the directory the controller starts from was *proven* writable
    /// only by administrators, so a persisted elevated task may point at it.
    #[serde(default)]
    pub location_protected: bool,
    /// The directory that verdict describes, when it could be resolved.
    #[serde(default)]
    pub location_directory: Option<String>,
    /// Actionable reason the current location may not host a persisted
    /// controller, or why it could not be proven safe.
    #[serde(default)]
    pub location_issue: Option<String>,
    pub issues: Vec<String>,
}

impl AutostartReport {
    fn for_snapshot(snapshot: &TaskSnapshot, status: AutostartStatus, registered: bool) -> Self {
        Self {
            task_name: TASK_NAME.to_owned(),
            status,
            registered,
            command: snapshot.command.clone(),
            run_level: snapshot.run_level.clone(),
            trigger: snapshot.trigger.clone(),
            location_protected: false,
            location_directory: None,
            location_issue: None,
            issues: Vec::new(),
        }
    }

    fn missing() -> Self {
        Self {
            task_name: TASK_NAME.to_owned(),
            status: AutostartStatus::Missing,
            registered: false,
            command: None,
            run_level: None,
            trigger: None,
            location_protected: false,
            location_directory: None,
            location_issue: None,
            issues: Vec::new(),
        }
    }

    /// Nothing is registered: the report describes an absent task, not the
    /// snapshot that was taken before an elevated helper ran.
    ///
    /// Carrying the pre-removal snapshot forward would claim a `command` and a
    /// `runLevel` for a task that no longer exists, and reporting `Removed`
    /// without re-reading the store would claim a removal that may not have
    /// happened.
    fn removed() -> Self {
        let mut report = Self::missing();
        report.status = AutostartStatus::Removed;
        report
    }

    /// Attaches the location verdict, and surfaces its reason through `issues`
    /// so existing consumers (`winter doctor`, `winter install`'s warning) show
    /// it without having to know the new fields.
    fn with_location(mut self, location: &LocationAssessment) -> Self {
        self.location_protected = location.protected;
        self.location_directory = location.directory.clone();
        self.location_issue = location.issue.clone();
        if let Some(issue) = &location.issue {
            self.issues.push(issue.clone());
        }
        self
    }

    fn foreign(snapshot: &TaskSnapshot) -> Self {
        let mut report = Self::for_snapshot(snapshot, AutostartStatus::Foreign, false);
        report.issues.push(format!(
            "a scheduled task named {TASK_NAME} already exists but is not managed by Winter; \
             rename or delete it, then run `winter autostart enable` again"
        ));
        report
    }

    /// Winter owns the task, but it would start something other than the
    /// binary that is asking — usually because the install moved.
    fn stale(snapshot: &TaskSnapshot, expected: &Path) -> Self {
        let mut report = Self::for_snapshot(snapshot, AutostartStatus::Stale, false);
        report.issues.push(format!(
            "the registered task starts {}, not {}; run `winter autostart enable` to point it at this binary",
            snapshot.command.as_deref().unwrap_or("<unknown>"),
            expected.display()
        ));
        report
    }

    /// Report for a task that could not be touched, with the reason attached.
    #[must_use]
    pub fn skipped(reason: &str) -> Self {
        let mut report = Self::missing();
        report.status = AutostartStatus::Skipped;
        report.issues.push(reason.to_owned());
        report
    }

    /// True when the report proves that a reboot will restore the controller.
    #[must_use]
    pub const fn persistent(&self) -> bool {
        self.registered
            && matches!(
                self.status,
                AutostartStatus::Registered | AutostartStatus::Unchanged | AutostartStatus::Updated
            )
    }
}

/// Read-only autostart state; never asks for elevation.
///
/// A task Winter owns but that no longer starts *this* binary is reported as
/// [`AutostartStatus::Stale`] rather than registered: a moved or deleted
/// install would otherwise look persistent while every sign-in fails with
/// "the system cannot find the file specified".
///
/// The report always carries the location verdict as well, so a task that *is*
/// registered from a writable directory — the state every install lands in
/// before it is moved under `%ProgramFiles%` — is visible here instead of only
/// when someone tries to enable it.
pub fn status() -> AppResult<AutostartReport> {
    let location = LocationAssessment::inspect();
    let report = match current_task()? {
        Some(snapshot) if !snapshot.managed => AutostartReport::foreign(&snapshot),
        Some(snapshot) => match controller_command() {
            Ok(command) if !snapshot.matches(&command) => {
                AutostartReport::stale(&snapshot, &command)
            }
            _ => AutostartReport::for_snapshot(&snapshot, AutostartStatus::Registered, true),
        },
        None => AutostartReport::missing(),
    };
    Ok(report.with_location(&location))
}

/// Registers or repairs the logon task, asking for elevation when needed.
///
/// `via_elevation` marks a helper process that Windows started through UAC;
/// such a process must never try to elevate itself again.
///
/// The location is verified *before* anything else, including before the UAC
/// prompt: registering the task first and refusing afterwards would already
/// have created the elevated-at-logon entry the refusal exists to prevent, and
/// asking for approval for a registration that must not happen wastes the one
/// prompt the user is willing to answer.
pub fn enable(via_elevation: bool) -> AppResult<AutostartReport> {
    let command = controller_command()?;
    let location = LocationAssessment::verify(&command)?;
    Ok(enable_at(via_elevation, &command)?.with_location(&location))
}

/// Registers or repairs the logon task for a location already proven safe.
fn enable_at(via_elevation: bool, command: &Path) -> AppResult<AutostartReport> {
    let existing = current_task()?;
    if let Some(snapshot) = &existing
        && !snapshot.managed
    {
        return Ok(AutostartReport::foreign(snapshot));
    }

    if let Some(snapshot) = &existing
        && snapshot.matches(command)
    {
        return Ok(AutostartReport::for_snapshot(
            snapshot,
            AutostartStatus::Unchanged,
            true,
        ));
    }
    let replacing = existing.is_some();

    if !elevated()? {
        if via_elevation {
            return Err(AppError::InvalidConfiguration(
                "the helper process is still not elevated; the scheduled task cannot be registered"
                    .to_owned(),
            ));
        }
        match run_current_process_elevated_and_wait(&["autostart", "enable", VIA_ELEVATION_ARG]) {
            Ok(0) => {}
            Ok(code) => {
                return Err(AppError::Native(format!(
                    "the elevated autostart helper exited with status {code}"
                )));
            }
            Err(PlatformError::ElevationDeclined) => {
                return Ok(AutostartReport::skipped(
                    "administrator approval was dismissed, so persistence is not active; \
                     run `winter autostart enable` from an elevated shell to finish the setup",
                ));
            }
            Err(error) => return Err(AppError::platform(error)),
        }
        // The helper did the write; re-read the task so the report describes
        // the on-disk state instead of the request.
        return match current_task()? {
            Some(snapshot) if snapshot.managed => Ok(AutostartReport::for_snapshot(
                &snapshot,
                AutostartStatus::Registered,
                true,
            )),
            Some(snapshot) => Ok(AutostartReport::foreign(&snapshot)),
            None => Err(AppError::OperationIncomplete(
                "the elevated helper reported success but no scheduled task was created".to_owned(),
            )),
        };
    }

    let spec = desired_spec(command)?;
    register_task(TASK_NAME, &task_xml(&spec)).map_err(AppError::platform)?;
    let installed = current_task()?.ok_or_else(|| {
        AppError::OperationIncomplete(
            "the scheduled task was created but cannot be read back".to_owned(),
        )
    })?;
    let status = if replacing {
        AutostartStatus::Updated
    } else {
        AutostartStatus::Registered
    };
    Ok(AutostartReport::for_snapshot(&installed, status, true))
}

/// Removes the managed task, asking for elevation when needed.
pub fn disable(via_elevation: bool) -> AppResult<AutostartReport> {
    let Some(snapshot) = current_task()? else {
        return Ok(AutostartReport::missing());
    };
    if !snapshot.managed {
        return Ok(AutostartReport::foreign(&snapshot));
    }

    if !elevated()? {
        if via_elevation {
            return Err(AppError::InvalidConfiguration(
                "the helper process is still not elevated; the scheduled task cannot be removed"
                    .to_owned(),
            ));
        }
        match run_current_process_elevated_and_wait(&["autostart", "disable", VIA_ELEVATION_ARG]) {
            Ok(0) => {}
            Ok(code) => {
                return Err(AppError::Native(format!(
                    "the elevated autostart helper exited with status {code}"
                )));
            }
            Err(PlatformError::ElevationDeclined) => {
                return Ok(AutostartReport::skipped(
                    "administrator approval was dismissed, so the logon task is still registered; \
                     run `winter autostart disable` from an elevated shell to remove it",
                ));
            }
            Err(error) => return Err(AppError::platform(error)),
        }
        // The helper ran in its own process and was waited on, so its exit
        // status says nothing about the store. Re-read the task and report what
        // is actually there: the pre-fix code reported `Removed` from the
        // snapshot taken *before* the UAC prompt, which claimed a removal the
        // helper might never have performed.
        return removal_report();
    }

    delete_task(TASK_NAME).map_err(AppError::platform)?;
    removal_report()
}

/// Reads the managed task back after a removal attempt.
fn removal_report() -> AppResult<AutostartReport> {
    match current_task() {
        Ok(observed) => Ok(report_removal(observed)),
        // The removal may well have succeeded; what failed is the proof. That is
        // an incomplete operation, not a successful one.
        Err(error) => Err(AppError::OperationIncomplete(format!(
            "the scheduled task could not be read back after the removal, so its state is \
             unverified: {error}"
        ))),
    }
}

/// Maps the task state observed after a removal attempt into a report.
fn report_removal(observed: Option<TaskSnapshot>) -> AutostartReport {
    match observed {
        None => AutostartReport::removed(),
        Some(snapshot) if snapshot.managed => {
            let mut report =
                AutostartReport::for_snapshot(&snapshot, AutostartStatus::Registered, true);
            report.issues.push(
                "the scheduled task is still registered after the removal finished; run \
                 `winter autostart disable` from an elevated shell to remove it"
                    .to_owned(),
            );
            report
        }
        Some(snapshot) => AutostartReport::foreign(&snapshot),
    }
}

/// Best-effort registration used by `winter install`.
///
/// The Windows Terminal integration is already in place at this point, so a
/// failure to register persistence must not fail the whole install: it is
/// reported instead, and `winter doctor` keeps showing the truth.
#[must_use]
pub fn ensure_enabled() -> AutostartReport {
    enable(false).unwrap_or_else(|error| AutostartReport::skipped(&error.to_string()))
}

/// Best-effort removal used by `winter uninstall`.
#[must_use]
pub fn ensure_disabled() -> AutostartReport {
    disable(false).unwrap_or_else(|error| AutostartReport::skipped(&error.to_string()))
}

/// Like [`status`], but never fails: a broken task store is reported instead.
#[must_use]
pub fn status_or_skipped() -> AutostartReport {
    status().unwrap_or_else(|error| AutostartReport::skipped(&error.to_string()))
}

fn elevated() -> AppResult<bool> {
    is_current_process_elevated().map_err(AppError::platform)
}

/// Whether the directory the controller starts from may host a persisted,
/// elevated executable, and why not when it may not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocationAssessment {
    protected: bool,
    directory: Option<String>,
    issue: Option<String>,
}

impl LocationAssessment {
    /// Inspects the executable this process would register.
    ///
    /// Infallible by construction: the read-only commands must still describe
    /// the task on disk when the location cannot be proven safe, so every
    /// failure becomes part of the verdict instead of an error.
    fn inspect() -> Self {
        match controller_command() {
            Ok(command) => match check_path_safety(&command) {
                Ok(safety) => Self::from_safety(&command, &safety),
                Err(error) => Self::refused(
                    Some(&command),
                    format!(
                        "the permissions on the controller's location could not be verified, so \
                         it cannot be trusted for a persisted controller: {error}"
                    ),
                ),
            },
            Err(error) => Self::refused(
                None,
                format!("the executable this process would persist could not be resolved: {error}"),
            ),
        }
    }

    /// The verifying half: an unsafe or unprovable location is an error whose
    /// message is the verdict, including how to fix it.
    ///
    /// This must stay a hard failure. Returning the verdict as a value here is
    /// what let `enable` register a task for a user-writable executable while
    /// the report already knew the location was unsafe.
    fn verify(command: &Path) -> AppResult<Self> {
        let safety = require_protected_path(command).map_err(AppError::platform)?;
        Ok(Self::from_safety(command, &safety))
    }

    /// Turns a successful inspection into an assessment.
    fn from_safety(command: &Path, safety: &PathSafety) -> Self {
        if safety.is_protected() {
            Self {
                protected: true,
                directory: Self::directory_of(Some(command)),
                issue: None,
            }
        } else {
            Self::refused(Some(command), safety.summary())
        }
    }

    fn refused(command: Option<&Path>, reason: String) -> Self {
        Self {
            protected: false,
            directory: Self::directory_of(command),
            issue: Some(reason),
        }
    }

    fn directory_of(command: Option<&Path>) -> Option<String> {
        command
            .and_then(Path::parent)
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.display().to_string())
    }
}

/// Resolves the executable the logon task should start.
///
/// `winterd.exe` is preferred: it is the hidden controller entry point and
/// carries its own diagnostics. A build without it falls back to `winter`
/// itself, whose default command is `launch`.
///
/// The result is resolved to its long form before it is written into the task:
/// a process started through an 8.3 alias (`C:\Users\ADMINI~1\…`) reports that
/// alias from `current_exe`, and storing it would make every later launch from
/// the long path look like a different program and rewrite the task.
fn controller_command() -> AppResult<PathBuf> {
    let current = env::current_exe()
        .map_err(|error| AppError::io("resolve current executable", PathBuf::from("."), error))?;
    let daemon = current.with_file_name("winterd.exe");
    let selected = if daemon.is_file() { daemon } else { current };
    Ok(long_path(&selected))
}

/// Resolves `path` to an absolute long path, without the `\\?\` prefix.
///
/// Falls back to the input when the file cannot be resolved, so a missing
/// binary still produces a readable task definition instead of an error.
fn long_path(path: &Path) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    let rendered = resolved.display().to_string();
    match rendered.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

fn desired_spec(command: &Path) -> AppResult<TaskSpec> {
    let working_directory = command
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let user_sid = current_user_sid_string().map_err(AppError::platform)?;
    Ok(TaskSpec {
        description: TASK_DESCRIPTION.to_owned(),
        command: command.to_path_buf(),
        working_directory,
        user_sid,
        logon_delay: LOGON_DELAY.to_owned(),
    })
}

/// What the Task Scheduler currently holds under our name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskSnapshot {
    command: Option<String>,
    run_level: Option<String>,
    trigger: Option<String>,
    managed: bool,
}

impl TaskSnapshot {
    fn parse(xml: &str) -> Self {
        Self {
            command: element_text(xml, "Command"),
            run_level: element_text(xml, "RunLevel"),
            trigger: xml.contains("<LogonTrigger>").then(|| "atLogon".to_owned()),
            managed: xml.contains(OWNERSHIP_MARKER),
        }
    }

    /// True when this task would start the controller we are running from.
    fn matches(&self, command: &Path) -> bool {
        self.managed
            && self.trigger.as_deref() == Some("atLogon")
            && self.run_level.as_deref() == Some("HighestAvailable")
            && self
                .command
                .as_deref()
                .is_some_and(|registered| same_path(registered, command))
    }
}

fn current_task() -> AppResult<Option<TaskSnapshot>> {
    let xml = query_task_xml(TASK_NAME).map_err(AppError::platform)?;
    Ok(xml.as_deref().map(TaskSnapshot::parse))
}

/// Returns the text of the first `<tag>…</tag>` element, trimmed.
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_owned())
}

/// Compares a registered command against the executable we are running from.
///
/// Both sides are resolved first, so an 8.3 alias, a `\\?\` prefix, a
/// different separator, or a different letter case all still describe the same
/// program. Text normalization is the fallback for a registered path that no
/// longer exists — that task is stale and should be rewritten anyway.
fn same_path(registered: &str, desired: &Path) -> bool {
    if let Some(resolved) = resolved_key(Path::new(registered))
        && let Some(expected) = resolved_key(desired)
    {
        return resolved == expected;
    }
    normalize(registered) == normalize(&desired.display().to_string())
}

/// Canonical, comparable spelling of an existing path.
fn resolved_key(path: &Path) -> Option<String> {
    let resolved = std::fs::canonicalize(path).ok()?;
    Some(normalize(&resolved.display().to_string()))
}

fn normalize(path: &str) -> String {
    path.replace('/', r"\")
        .trim_end_matches('\\')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const REGISTERED_XML: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Winter controller autostart (managed by `winter install`)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>S-1-5-21-1</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>HUITTOYO\Administrator</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\dev\AppData\Local\Winter\winterd.exe</Command>
      <WorkingDirectory>C:\Users\dev\AppData\Local\Winter</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#;

    /// The definition above is the shape Task Scheduler writes back for a task
    /// Winter registered: the principal is canonicalized to `DOMAIN\User`
    /// while the trigger keeps the SID it was created with. Re-running
    /// `winter autostart enable` must therefore report "already correct"
    /// instead of rewriting the task on every install.
    #[test]
    fn a_managed_task_parses_into_a_matching_snapshot() {
        let snapshot = TaskSnapshot::parse(REGISTERED_XML);

        assert!(snapshot.managed);
        assert_eq!(
            snapshot.command.as_deref(),
            Some(r"C:\Users\dev\AppData\Local\Winter\winterd.exe")
        );
        assert_eq!(snapshot.run_level.as_deref(), Some("HighestAvailable"));
        assert_eq!(snapshot.trigger.as_deref(), Some("atLogon"));
        assert!(snapshot.matches(Path::new(r"c:/users/DEV/appdata/local/winter/WINTERD.EXE")));
    }

    /// Windows keeps an 8.3 alias for most paths and a process started through
    /// it reports the alias as its own executable, so a naive string compare
    /// would rewrite the task on every launch.
    #[test]
    fn path_comparison_sees_through_aliases_and_prefixes() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let file = temp.path().join("winterd.exe");
        std::fs::write(&file, b"").expect("fixture should be written");

        let long = long_path(&file);
        assert!(
            !long.display().to_string().starts_with(r"\\?\"),
            "the verbatim prefix must be stripped before it reaches the task: {long:?}"
        );
        assert!(
            file.is_file(),
            "the resolved path must still exist: {long:?}"
        );

        let verbatim = format!(r"\\?\{}", long.display());
        assert!(
            same_path(&verbatim, &file),
            "a verbatim spelling must match the plain one"
        );
        assert!(
            same_path(
                &long.display().to_string().replace('\\', "/").to_uppercase(),
                &file
            ),
            "separators and case never distinguish two paths"
        );
        assert!(
            !same_path(r"C:\definitely\not\here\winterd.exe", &file),
            "a different program must not be mistaken for ours"
        );
    }

    /// A same-named task created by another tool must never be claimed.
    #[test]
    fn a_foreign_task_is_never_treated_as_managed() {
        let foreign = REGISTERED_XML.replace(OWNERSHIP_MARKER, "Nightly backup");
        let snapshot = TaskSnapshot::parse(&foreign);

        assert!(!snapshot.managed);
        assert!(!snapshot.matches(Path::new(r"C:\Users\dev\AppData\Local\Winter\winterd.exe")));

        let report = AutostartReport::foreign(&snapshot);
        assert_eq!(report.status, AutostartStatus::Foreign);
        assert!(!report.registered);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains("not managed")),
            "a foreign task must be explained: {report:?}"
        );
    }

    /// A task that exists but points at a stale path must be repaired.
    #[test]
    fn a_stale_command_does_not_match() {
        let snapshot = TaskSnapshot::parse(REGISTERED_XML);

        assert!(!snapshot.matches(Path::new(r"C:\Other\winterd.exe")));
    }

    /// A moved or deleted install leaves the task pointing at nothing. That is
    /// not "registered": every sign-in would fail with "the system cannot find
    /// the file specified", so the report has to say so.
    #[test]
    fn a_task_that_starts_something_else_is_reported_stale() {
        let snapshot = TaskSnapshot::parse(REGISTERED_XML);
        let expected = Path::new(r"C:\Program Files\Winter\winterd.exe");

        let report = AutostartReport::stale(&snapshot, expected);

        assert_eq!(report.status, AutostartStatus::Stale);
        assert!(
            !report.registered,
            "a task that cannot start this binary is not persistent"
        );
        assert!(!report.persistent());
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains("run `winter autostart enable`")),
            "the repair path must be spelled out: {report:?}"
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains(expected.to_str().unwrap_or_default())),
            "the issue must name the binary the caller expected: {report:?}"
        );
    }

    #[test]
    fn a_task_without_the_highest_run_level_does_not_match() {
        let downgraded = REGISTERED_XML.replace("HighestAvailable", "LeastPrivilege");
        let snapshot = TaskSnapshot::parse(&downgraded);

        assert!(snapshot.managed);
        assert!(
            !snapshot.matches(Path::new(r"C:\Users\dev\AppData\Local\Winter\winterd.exe")),
            "a task that would trigger UAC at logon must be repaired"
        );
    }

    #[test]
    fn extraction_is_tolerant_of_missing_elements() {
        assert_eq!(element_text("<a><b>hi</b></a>", "b").as_deref(), Some("hi"));
        assert_eq!(
            element_text("<a><b> hi </b></a>", "b").as_deref(),
            Some("hi")
        );
        assert_eq!(element_text("<a/>", "b"), None);
    }

    /// The helper switch exists in two spellings — clap's and the command
    /// line's — and drift between them would silently drop the guard against
    /// recursive UAC launches.
    #[test]
    fn the_helper_switch_spellings_agree() {
        assert_eq!(format!("--{VIA_ELEVATION_LONG}"), VIA_ELEVATION_ARG);
    }

    #[test]
    fn skipped_reports_explain_themselves_and_are_not_persistent() {
        let report = AutostartReport::skipped("administrator approval was dismissed");

        assert_eq!(report.status, AutostartStatus::Skipped);
        assert_eq!(report.task_name, TASK_NAME);
        assert!(!report.persistent());
        assert_eq!(report.issues.len(), 1);
    }

    #[test]
    fn a_registered_report_proves_persistence() {
        let snapshot = TaskSnapshot::parse(REGISTERED_XML);
        let report = AutostartReport::for_snapshot(&snapshot, AutostartStatus::Unchanged, true);

        assert!(report.persistent());
        assert!(report.registered);
    }

    #[test]
    fn the_task_definition_names_the_current_account_and_the_controller() {
        let spec = TaskSpec {
            description: TASK_DESCRIPTION.to_owned(),
            command: PathBuf::from(r"C:\Winter\winterd.exe"),
            working_directory: PathBuf::from(r"C:\Winter"),
            user_sid: "S-1-5-21-1-2-3-500".to_owned(),
            logon_delay: LOGON_DELAY.to_owned(),
        };
        let xml = task_xml(&spec);

        assert!(
            xml.contains(OWNERSHIP_MARKER),
            "the marker must be present: {xml}"
        );
        assert!(xml.contains("<UserId>S-1-5-21-1-2-3-500</UserId>"), "{xml}");
        assert!(
            xml.contains("<Command>C:\\Winter\\winterd.exe</Command>"),
            "{xml}"
        );
    }

    /// The read-only status path must work in any shell, elevated or not.
    #[test]
    fn status_is_read_only_and_returns_a_task_name() {
        let report = status_or_skipped();

        assert_eq!(report.task_name, TASK_NAME);
        if !report.registered {
            assert!(
                matches!(
                    report.status,
                    AutostartStatus::Missing
                        | AutostartStatus::Foreign
                        | AutostartStatus::Stale
                        | AutostartStatus::Skipped
                ),
                "an unregistered task must not claim a live status: {report:?}"
            );
        }
        if report.status == AutostartStatus::Stale {
            assert!(
                !report.issues.is_empty(),
                "a stale task must explain what to repair: {report:?}"
            );
        }
    }

    /// The read-only status must never claim a location is protected without
    /// evidence: a refusal always carries the reason, so the operator can act on
    /// it instead of reading a bare `false`.
    #[test]
    fn the_status_report_never_claims_a_protected_location_without_a_verdict() {
        let report = status_or_skipped();

        if report.location_protected {
            assert!(
                report.location_issue.is_none(),
                "a protected location has nothing to explain: {report:?}"
            );
        } else {
            assert!(
                report.location_issue.is_some() || report.status == AutostartStatus::Skipped,
                "an unprotected location must say why: {report:?}"
            );
        }
    }

    /// The location verdict has to survive into the JSON the CLI prints, since
    /// that is how `winter autostart status` and `winter doctor` report it.
    #[test]
    fn the_location_verdict_is_part_of_the_json_contract() {
        let location = LocationAssessment {
            protected: false,
            directory: Some(r"C:\Users\dev\.cargo\bin".to_owned()),
            issue: Some("C:\\Users\\dev\\.cargo\\bin can be overwritten".to_owned()),
        };
        let report = AutostartReport::missing().with_location(&location);
        let json = serde_json::to_value(&report).expect("the report must serialize");

        assert_eq!(json["locationProtected"], serde_json::json!(false));
        assert_eq!(
            json["locationDirectory"],
            serde_json::json!(r"C:\Users\dev\.cargo\bin")
        );
        assert_eq!(
            json["locationIssue"],
            serde_json::json!("C:\\Users\\dev\\.cargo\\bin can be overwritten")
        );
        assert_eq!(
            report.issues.len(),
            1,
            "the reason must also be visible to consumers that only read `issues`"
        );

        let protected = AutostartReport::missing().with_location(&LocationAssessment {
            protected: true,
            directory: Some(r"C:\Program Files\Winter".to_owned()),
            issue: None,
        });
        assert!(protected.location_protected);
        assert!(protected.issues.is_empty());
    }

    /// The location check must actually refuse the default Cargo location: the
    /// test binary runs from a user-writable tree, so `enable` on it can only
    /// fail, and it must fail *before* any registration.
    #[test]
    fn an_account_writable_location_is_refused_before_registration() {
        let command = controller_command().expect("the test binary must resolve");
        let safety = crate::platform::windows::check_path_safety(&command)
            .expect("the test binary's location must be inspectable");
        assert!(
            !safety.is_protected(),
            "the test binary lives in a user-writable tree; a fixture that changed that would \
             invalidate this test: {safety:?}"
        );

        let error =
            require_protected_path(&command).expect_err("an account-writable path is refused");
        let message = error.to_string();

        assert!(
            message.contains("refusing to persist a logon task"),
            "the refusal must say what it refused: {message}"
        );
        assert!(
            message.contains("administrators can write"),
            "the refusal must say how to fix it: {message}"
        );
    }

    /// `winter install` must not silently register. When the location is
    /// refused, the install path has to report a `Skipped` report carrying the
    /// refusal — not an error, and above all not a task.
    ///
    /// The guard is deliberate: on a machine where the test binary happens to
    /// live in a protected directory this call would really register the task,
    /// and a test suite must never do that.
    #[test]
    fn ensure_enabled_reports_a_skipped_install_for_an_unprotected_location() {
        let command = controller_command().expect("the test binary must resolve");
        let safety =
            check_path_safety(&command).expect("the test binary's location must be readable");
        if safety.is_protected() {
            eprintln!("skipping: this test binary lives in a protected location");
            return;
        }

        let report = ensure_enabled();

        assert_eq!(report.status, AutostartStatus::Skipped, "{report:?}");
        assert!(!report.registered, "nothing may be registered: {report:?}");
        assert!(!report.persistent());
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains("refusing to persist a logon task")),
            "the skip must carry the refusal and its remedy: {report:?}"
        );
    }

    /// The verifying entry point must *fail*, not merely describe: a verdict
    /// carried as a value is what let `enable` register a task for a
    /// user-writable executable while the report already knew it was unsafe.
    #[test]
    fn verifying_an_account_writable_location_is_an_error() {
        let command = controller_command().expect("the test binary must resolve");

        let error = LocationAssessment::verify(&command)
            .expect_err("an account-writable location must not verify");

        assert!(
            error
                .to_string()
                .contains("refusing to persist a logon task"),
            "the failure must be the refusal itself: {error}"
        );
    }

    /// A removal that did not take effect must be reported from the disk, not
    /// from the snapshot taken before the elevated helper ran. This is the bug
    /// the read-back fixes: the old code returned `Removed` either way.
    #[test]
    fn a_task_that_survives_the_removal_is_reported_as_still_registered() {
        let removed = report_removal(None);

        assert_eq!(removed.status, AutostartStatus::Removed);
        assert!(!removed.registered);
        assert!(
            removed.command.is_none(),
            "nothing is registered, so no command may be described: {removed:?}"
        );
        assert!(removed.issues.is_empty());

        let survived = report_removal(Some(TaskSnapshot::parse(REGISTERED_XML)));

        assert_eq!(survived.status, AutostartStatus::Registered);
        assert!(
            survived.registered,
            "a task still on disk is still persistent: {survived:?}"
        );
        assert!(
            survived.persistent(),
            "a surviving task still restores the controller: {survived:?}"
        );
        assert!(
            survived
                .issues
                .iter()
                .any(|issue| issue.contains("still registered")),
            "the survivor must explain the repair: {survived:?}"
        );
    }

    /// A same-named task that is not ours, reappearing during the removal, must
    /// still be reported as foreign rather than as a successful removal.
    #[test]
    fn a_foreign_task_observed_after_the_removal_is_not_reported_as_removed() {
        let foreign =
            TaskSnapshot::parse(&REGISTERED_XML.replace(OWNERSHIP_MARKER, "Nightly backup"));

        let report = report_removal(Some(foreign));

        assert_eq!(report.status, AutostartStatus::Foreign);
        assert!(!report.registered);
    }
}
