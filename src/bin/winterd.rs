#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{Location, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use winterminalp::integration::{IntegrationConfig, doctor};
use winterminalp::platform::windows::{launch_windows_terminal, relaunch_current_process_elevated};
use winterminalp::{
    AppError, AppResult, ControllerConfig, ControllerOptions, bridge_is_ready,
    config::{default_app_data_dir, default_config_path},
    run_controller,
};

const LAST_ERROR_FILE: &str = "last-error.log";
const FALLBACK_ERROR_FILE: &str = "winterminalp-last-error.log";

fn main() -> ExitCode {
    install_panic_hook();
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_last_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn execute() -> AppResult<()> {
    let launch_mode = parse_launch_mode()?;
    let elevation_arguments = launch_mode.elevation_arguments();
    if relaunch_current_process_elevated(elevation_arguments)
        .map_err(|error| AppError::Native(error.to_string()))?
    {
        return Ok(());
    }
    let integration = IntegrationConfig::from_environment()?;
    let doctor_report = doctor(&integration)?;
    if !bridge_is_ready(&doctor_report) {
        return Err(AppError::InvalidConfiguration(
            "Windows Terminal integration is not ready; run `winter doctor` and `winter install` first"
                .to_owned(),
        ));
    }

    let config = ControllerConfig::load_or_create(&default_config_path()?)?;
    let should_launch = launch_mode.should_launch(config.launch_terminal_on_start);
    let result = run_controller(
        &config,
        ControllerOptions {
            launch_terminal: should_launch,
            bridge_ready: true,
            ..ControllerOptions::default()
        },
    );

    match result {
        Err(AppError::ControllerAlreadyRunning) if should_launch => {
            let _child =
                launch_windows_terminal().map_err(|error| AppError::Native(error.to_string()))?;
            Ok(())
        }
        Err(AppError::ControllerAlreadyRunning) => Ok(()),
        result => result.map(|_| ()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonLaunchMode {
    Config,
    Force,
    Suppress,
}

impl DaemonLaunchMode {
    const fn should_launch(self, configured: bool) -> bool {
        match self {
            Self::Config => configured,
            Self::Force => true,
            Self::Suppress => false,
        }
    }

    const fn elevation_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Config => &[],
            Self::Force => &["--launch"],
            Self::Suppress => &["--no-launch"],
        }
    }
}

fn parse_launch_mode() -> AppResult<DaemonLaunchMode> {
    let mut launch_mode = DaemonLaunchMode::Config;
    for argument in env::args_os().skip(1) {
        match argument.to_string_lossy().as_ref() {
            "--launch" if launch_mode == DaemonLaunchMode::Config => {
                launch_mode = DaemonLaunchMode::Force;
            }
            "--no-launch" if launch_mode == DaemonLaunchMode::Config => {
                launch_mode = DaemonLaunchMode::Suppress;
            }
            _ => {
                return Err(AppError::InvalidConfiguration(format!(
                    "unknown or conflicting daemon argument: {}",
                    argument.to_string_lossy()
                )));
            }
        }
    }
    Ok(launch_mode)
}

/// Installs the panic hook used for the whole daemon lifetime.
///
/// `winterd` is linked with `windows_subsystem = "windows"`, so it has no
/// console and a panic would otherwise be completely invisible in release
/// builds. The hook routes the payload through the same last-error mechanism
/// as `write_last_error`, chains to whatever hook was already installed, and
/// then calls `std::process::abort()`: a panicked controller process cannot
/// keep driving panes, and aborting makes the crash observable to the
/// launcher as a non-successful exit instead of leaving a half-dead daemon
/// running behind a silently dead thread.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_message(&panic_message(info));
        previous(info);
        std::process::abort();
    }));
}

/// Renders a panic payload and its source location into one log line.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    format_panic(info.location(), &panic_description(info.payload()))
}

/// Extracts the human-readable text from a panic payload.
fn panic_description(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Formats a panic line, including the source location when one is known.
fn format_panic(location: Option<&Location<'_>>, description: &str) -> String {
    match location {
        Some(location) => format!(
            "winterd panicked at {}:{}:{}: {description}",
            location.file(),
            location.line(),
            location.column()
        ),
        None => format!("winterd panicked: {description}"),
    }
}

fn write_last_error(error: &AppError) {
    write_message(&error.to_string());
}

/// Writes `message` to the first candidate path that accepts it.
///
/// The primary location is the app-data directory under `LOCALAPPDATA`. When
/// that directory cannot be created, or the file cannot be opened, written or
/// synced, the attempt falls back to the system temp directory. Only when
/// both attempts fail is the message dropped.
fn write_message(message: &str) {
    write_message_to_paths(
        message,
        &error_log_paths(
            default_app_data_dir().ok(),
            env::temp_dir().join(FALLBACK_ERROR_FILE),
        ),
    );
}

/// Selects the primary and fallback last-error log paths, in that order.
///
/// Both candidate locations are injected so path selection stays a pure
/// function. When the primary directory is unavailable (no `LOCALAPPDATA`),
/// the fallback path is used for both entries so the message still has a
/// destination.
fn error_log_paths(primary_dir: Option<PathBuf>, fallback: PathBuf) -> [PathBuf; 2] {
    let primary = match primary_dir {
        Some(directory) => directory.join(LAST_ERROR_FILE),
        None => fallback.clone(),
    };
    [primary, fallback]
}

/// Attempts every candidate path in order, stopping at the first success.
fn write_message_to_paths(message: &str, paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| try_write_message(path, message))
}

/// Writes `message` plus a trailing newline to `path`, reporting success.
fn try_write_message(path: &Path, message: &str) -> bool {
    if let Some(directory) = path.parent() {
        if fs::create_dir_all(directory).is_err() {
            return false;
        }
    }
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    if writeln!(file, "{message}").is_err() {
        return false;
    }
    file.sync_all().is_ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic::Location;
    use std::path::PathBuf;

    use super::{
        DaemonLaunchMode, FALLBACK_ERROR_FILE, LAST_ERROR_FILE, error_log_paths, format_panic,
        panic_description, write_message_to_paths,
    };

    #[test]
    fn explicit_daemon_launch_mode_overrides_config_only_when_requested() {
        assert!(DaemonLaunchMode::Config.should_launch(true));
        assert!(!DaemonLaunchMode::Config.should_launch(false));
        assert!(DaemonLaunchMode::Force.should_launch(false));
        assert!(!DaemonLaunchMode::Suppress.should_launch(true));
    }

    #[test]
    fn primary_dir_yields_app_data_log_before_temp_fallback() {
        let primary_dir = PathBuf::from(r"C:\invalid\readonly\WinTerminalP");
        let fallback = PathBuf::from(r"C:\temp\winterminalp-last-error.log");
        let [primary, chosen_fallback] =
            error_log_paths(Some(primary_dir.clone()), fallback.clone());
        assert_eq!(primary, primary_dir.join(LAST_ERROR_FILE));
        assert_eq!(chosen_fallback, fallback);
        assert_ne!(primary, fallback);
    }

    #[test]
    fn missing_primary_dir_falls_back_to_temp_path() {
        let fallback = PathBuf::from(r"C:\temp\winterminalp-last-error.log");
        let [primary, chosen_fallback] = error_log_paths(None, fallback.clone());
        assert_eq!(primary, fallback);
        assert_eq!(primary, chosen_fallback);
        assert_eq!(
            primary.file_name().and_then(|name| name.to_str()),
            Some(FALLBACK_ERROR_FILE)
        );
    }

    #[test]
    fn message_lands_in_fallback_when_primary_dir_cannot_be_created() {
        let base = scratch_dir("fallback-on-uncreatable-primary");
        let blocker = base.join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let primary = blocker.join(LAST_ERROR_FILE);
        let fallback = base.join("fallback.log");

        assert!(write_message_to_paths(
            "primary unavailable",
            &[primary.clone(), fallback.clone()]
        ));
        assert!(!primary.exists());
        assert_eq!(
            fs::read_to_string(&fallback).unwrap(),
            "primary unavailable\n"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn message_stops_at_first_writable_path() {
        let base = scratch_dir("stops-at-first-success");
        let first = base.join("first.log");
        let second = base.join("second.log");

        assert!(write_message_to_paths(
            "written once",
            &[first.clone(), second.clone()]
        ));
        assert_eq!(fs::read_to_string(&first).unwrap(), "written once\n");
        assert!(!second.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn all_paths_failing_reports_no_write() {
        let base = scratch_dir("all-paths-failing");
        let blocker = base.join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let blocked = blocker.join(LAST_ERROR_FILE);

        assert!(!write_message_to_paths(
            "lost",
            &[blocked.clone(), blocked.clone()]
        ));
        assert!(!blocked.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn panic_payload_description_supports_str_and_string() {
        assert_eq!(panic_description(&"boom"), "boom");
        assert_eq!(panic_description(&String::from("boom")), "boom");
        assert_eq!(panic_description(&42_u32), "non-string panic payload");
    }

    #[test]
    fn panic_text_includes_source_location_and_payload() {
        let location = Location::caller();
        let message = format_panic(Some(location), "kaboom");
        assert!(message.starts_with("winterd panicked at "));
        assert!(message.contains(&format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )));
        assert!(message.ends_with(": kaboom"));
    }

    #[test]
    fn panic_text_without_location_still_reports_payload() {
        assert_eq!(format_panic(None, "kaboom"), "winterd panicked: kaboom");
    }

    fn scratch_dir(label: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("winterd-tests-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }
}
