//! Per-user scheduled-task primitives used for persistent autostart.
//!
//! `schtasks.exe` is the only supported interface. It ships with Windows,
//! needs no COM initialization, and its `/XML` form gives explicit control
//! over the trigger, the principal, and the settings a `/TR` command line
//! cannot express — notably the run level and the battery policy that would
//! otherwise keep the controller from starting on a laptop.
//!
//! The module owns process spawning, file encoding, and text decoding only;
//! deciding *which* task to register belongs to [`crate::autostart`].

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::error::{PlatformError, PlatformResult};

/// Console-less spawn flag, so a task operation never flashes a window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Everything a managed task definition needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// Human-readable description; also carries the ownership marker that
    /// distinguishes a Winter-managed task from a same-named user task.
    pub description: String,
    /// Executable the task runs at logon.
    pub command: PathBuf,
    /// Working directory for that executable.
    pub working_directory: PathBuf,
    /// String SID of the account the task belongs to.
    pub user_sid: String,
    /// ISO-8601 delay applied after logon, e.g. `PT5S`.
    pub logon_delay: String,
}

/// Renders the Task Scheduler XML document for `spec`.
///
/// The definition is intentionally narrow: one logon trigger for the current
/// account, an interactive-token principal at the highest available run
/// level (so the elevated controller starts without a second UAC prompt), no
/// execution time limit, and no battery restrictions. `<Hidden>` stays
/// `false` on purpose — the task is visible in Task Scheduler so a user can
/// audit or remove it.
#[must_use]
pub fn task_xml(spec: &TaskSpec) -> String {
    let description = escape_xml(&spec.description);
    let command = escape_xml(&spec.command.display().to_string());
    let working_directory = escape_xml(&spec.working_directory.display().to_string());
    let user_sid = escape_xml(&spec.user_sid);
    let logon_delay = escape_xml(&spec.logon_delay);

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_sid}</UserId>
      <Delay>{logon_delay}</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user_sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <WorkingDirectory>{working_directory}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Creates or replaces the task `name` from a Task Scheduler XML document.
///
/// `schtasks /Create` accepts both UTF-16LE and UTF-8 input, but the task
/// scheduler service is happiest with the encoding it writes itself, so the
/// document is staged as UTF-16LE with a byte-order mark and removed again.
pub fn register_task(name: &str, xml: &str) -> PlatformResult<()> {
    let staging = staging_path();
    write_utf16(&staging, xml)?;
    let result = run(&[
        "/Create",
        "/TN",
        name,
        "/XML",
        &staging.to_string_lossy(),
        "/F",
    ]);
    let _ = fs::remove_file(&staging);
    result.map(|_| ())
}

/// Deletes the task `name`. Succeeds when it did not exist.
pub fn delete_task(name: &str) -> PlatformResult<()> {
    match run(&["/Delete", "/TN", name, "/F"]) {
        Ok(_) => Ok(()),
        // `/Delete` reports "cannot find the file specified" for an absent
        // task; callers treat a missing task as already removed.
        Err(error) if task_is_absent(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Returns the task's XML definition, or `None` when no such task exists.
pub fn query_task_xml(name: &str) -> PlatformResult<Option<String>> {
    match run(&["/Query", "/TN", name, "/XML"]) {
        Ok(stdout) => Ok(Some(stdout)),
        Err(error) if task_is_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// True when a `schtasks` failure means "this task does not exist".
///
/// `schtasks` localizes its messages, so the exit status is the only stable
/// signal: it returns 1 for both "not found" and a genuine failure. Callers
/// only reach this check after a successful spawn, and the follow-up
/// registration step surfaces any real failure with its own message.
fn task_is_absent(error: &PlatformError) -> bool {
    matches!(
        error,
        PlatformError::ScheduledTask {
            exit_code: Some(1),
            ..
        }
    )
}

fn run(arguments: &[&str]) -> PlatformResult<String> {
    let output = Command::new("schtasks.exe")
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|source| PlatformError::ScheduledTaskSpawn { source })?;

    let stdout = decode_console_text(&output.stdout);
    let stderr = decode_console_text(&output.stderr);
    if output.status.success() {
        return Ok(stdout);
    }

    Err(PlatformError::ScheduledTask {
        operation: operation_name(arguments).to_owned(),
        exit_code: output.status.code(),
        message: first_line(&stderr).unwrap_or_else(|| first_line(&stdout).unwrap_or_default()),
    })
}

fn operation_name(arguments: &[&str]) -> &'static str {
    match arguments.first().copied() {
        Some("/Create") => "created",
        Some("/Delete") => "removed",
        Some("/Query") => "queried",
        _ => "processed",
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Decodes text captured from `schtasks.exe`.
///
/// Console programs emit the active code page (or UTF-8 on current builds)
/// when redirected, and some builds still write UTF-16; a byte-order mark is
/// honoured so a non-ASCII task description never corrupts the report.
fn decode_console_text(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], to_unit: fn([u8; 2]) -> u16) -> String {
    let (pairs, _remainder) = bytes.as_chunks::<2>();
    let units = pairs.iter().copied().map(to_unit).collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

fn staging_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("winter-task-{}.xml", std::process::id()))
}

/// Writes `xml` as UTF-16LE with a byte-order mark.
fn write_utf16(path: &Path, xml: &str) -> PlatformResult<()> {
    let mut bytes = Vec::with_capacity(xml.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|source| PlatformError::ScheduledTaskStaging {
        path: path.to_path_buf(),
        source,
    })
}

/// Escapes the five XML metacharacters.
fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn spec(description: &str, command: &str, sid: &str) -> TaskSpec {
        TaskSpec {
            description: description.to_owned(),
            command: PathBuf::from(command),
            working_directory: PathBuf::from(r"C:\Program Files\Winter"),
            user_sid: sid.to_owned(),
            logon_delay: "PT5S".to_owned(),
        }
    }

    #[test]
    fn task_xml_carries_the_trigger_principal_and_action() {
        let xml = task_xml(&spec(
            "Winter marker",
            r"C:\Program Files\Winter\winterd.exe",
            "S-1-5-21-1-2-3-500",
        ));

        assert!(xml.contains("<LogonTrigger>"), "{xml}");
        assert!(xml.contains("<UserId>S-1-5-21-1-2-3-500</UserId>"), "{xml}");
        assert!(
            xml.contains("<RunLevel>HighestAvailable</RunLevel>"),
            "{xml}"
        );
        assert!(
            xml.contains("<LogonType>InteractiveToken</LogonType>"),
            "the task must run in the user's interactive session: {xml}"
        );
        assert!(
            xml.contains("<Command>C:\\Program Files\\Winter\\winterd.exe</Command>"),
            "{xml}"
        );
        assert!(
            xml.contains("<WorkingDirectory>C:\\Program Files\\Winter</WorkingDirectory>"),
            "{xml}"
        );
        assert!(xml.contains("<Delay>PT5S</Delay>"), "{xml}");
    }

    /// A laptop must still start the controller on battery, and the task must
    /// not be killed when the machine switches to battery.
    #[test]
    fn task_xml_runs_on_battery_and_never_times_out() {
        let xml = task_xml(&spec(
            "Winter marker",
            r"C:\Winter\winterd.exe",
            "S-1-5-21-1-2-3-500",
        ));

        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"), "{xml}");
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"), "{xml}");
        assert!(xml.contains("<ExecutionTimeLimit>PT0S<"), "{xml}");
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew<"), "{xml}");
    }

    #[test]
    fn task_xml_stays_well_formed_for_hostile_paths() {
        let xml = task_xml(&spec(
            "Winter & <winter>",
            r"C:\A & B <x>\winterd.exe",
            "S-1-5-21-1",
        ));

        assert!(
            !xml.contains("A & B"),
            "raw ampersand leaked into XML: {xml}"
        );
        assert!(xml.contains("A &amp; B &lt;x&gt;"), "{xml}");
        assert!(xml.contains("Winter &amp; &lt;winter&gt;"), "{xml}");
    }

    #[test]
    fn console_text_decoding_honours_byte_order_marks() {
        assert_eq!(decode_console_text(b"plain"), "plain");

        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "中".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_console_text(&utf16), "中");

        let mut utf16_be = vec![0xFE, 0xFF];
        for unit in "中".encode_utf16() {
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decode_console_text(&utf16_be), "中");
    }

    #[test]
    fn an_absent_task_is_reported_as_none_rather_than_an_error() {
        let missing = format!("WinterAbsentProbe-{}", std::process::id());
        let queried = query_task_xml(&missing).expect("query must not fail on an absent task");
        assert!(
            queried.is_none(),
            "an absent task must be None: {queried:?}"
        );
        delete_task(&missing).expect("deleting an absent task is a no-op");
    }
}
