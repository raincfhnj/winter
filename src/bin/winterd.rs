#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{Location, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use clap::error::{Error as ClapError, ErrorKind};

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
    let cli = match DaemonCli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return handle_clap_error(error),
    };
    match execute(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_last_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: DaemonCli) -> AppResult<()> {
    let launch_mode = cli.launch_mode()?;
    let elevation_arguments = launch_mode.elevation_arguments();
    if relaunch_current_process_elevated(elevation_arguments).map_err(AppError::platform)? {
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
            let _child = launch_windows_terminal().map_err(AppError::platform)?;
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

/// Command line surface of the hidden `winterd` daemon.
///
/// The daemon is started with no arguments (double-click), with `--launch`
/// (spawned by `winter launch` and by the daemon's own UAC relaunch), or
/// with `--no-launch` (elevated suppression). Every other input is rejected
/// before the controller runs. `--help`/`-V` succeed and print to stdout so
/// typos stay distinguishable from valid modes without a console.
#[derive(Debug, Parser)]
#[command(name = "winterd", version, arg_required_else_help = false)]
struct DaemonCli {
    /// Open Windows Terminal on start regardless of the configured value.
    #[arg(long, action = clap::ArgAction::Count)]
    launch: u8,
    /// Keep Windows Terminal closed on start regardless of the configured value.
    #[arg(long, action = clap::ArgAction::Count)]
    no_launch: u8,
}

impl DaemonCli {
    /// Maps the parsed flags onto the daemon launch mode.
    ///
    /// Reproduces the pre-clap hand parser exactly: no flags resolve to the
    /// configured default, a single flag forces or suppresses the launch,
    /// and a repeated or combined flag is rejected as conflicting.
    fn launch_mode(&self) -> AppResult<DaemonLaunchMode> {
        match (self.launch, self.no_launch) {
            (0, 0) => Ok(DaemonLaunchMode::Config),
            (1, 0) => Ok(DaemonLaunchMode::Force),
            (0, 1) => Ok(DaemonLaunchMode::Suppress),
            _ => Err(AppError::InvalidConfiguration(
                "unknown or conflicting daemon argument: --launch/--no-launch".to_owned(),
            )),
        }
    }
}

/// Maps a failed clap parse onto the daemon's exit contract.
///
/// Help and version requests print to stdout — invisible for a
/// double-clicked GUI-subsystem binary, but harmless — and exit 0. Every
/// other parse failure is written to `last-error.log` and exits 1: the
/// daemon has no console for clap's default stderr message and exit code 2,
/// and the pre-clap hand parser logged invalid arguments the same way.
fn handle_clap_error(error: ClapError) -> ExitCode {
    if is_informational(error.kind()) {
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    let rendered = error.render();
    write_last_error(&AppError::InvalidConfiguration(format!(
        "invalid daemon arguments: {rendered}"
    )));
    ExitCode::FAILURE
}

/// Reports whether a clap error is a successful help or version request.
const fn is_informational(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    )
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

    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{
        DaemonCli, DaemonLaunchMode, FALLBACK_ERROR_FILE, LAST_ERROR_FILE, error_log_paths,
        format_panic, is_informational, panic_description, write_message_to_paths,
    };

    #[test]
    fn explicit_daemon_launch_mode_overrides_config_only_when_requested() {
        assert!(DaemonLaunchMode::Config.should_launch(true));
        assert!(!DaemonLaunchMode::Config.should_launch(false));
        assert!(DaemonLaunchMode::Force.should_launch(false));
        assert!(!DaemonLaunchMode::Suppress.should_launch(true));
    }

    #[test]
    fn no_arguments_resolve_to_config_launch_mode() {
        let cli = DaemonCli::try_parse_from(["winterd"]).unwrap();
        assert_eq!(cli.launch_mode().unwrap(), DaemonLaunchMode::Config);
    }

    #[test]
    fn launch_flag_forces_terminal_launch() {
        let cli = DaemonCli::try_parse_from(["winterd", "--launch"]).unwrap();
        assert_eq!(cli.launch_mode().unwrap(), DaemonLaunchMode::Force);
    }

    #[test]
    fn no_launch_flag_suppresses_terminal_launch() {
        let cli = DaemonCli::try_parse_from(["winterd", "--no-launch"]).unwrap();
        assert_eq!(cli.launch_mode().unwrap(), DaemonLaunchMode::Suppress);
    }

    #[test]
    fn unknown_arguments_are_rejected_before_execution() {
        let unknown = DaemonCli::try_parse_from(["winterd", "--lanch"]).unwrap_err();
        assert_eq!(unknown.kind(), ErrorKind::UnknownArgument);
        assert!(!is_informational(unknown.kind()));
        // `run` belongs to `winter`, never to `winterd`.
        let run_style = DaemonCli::try_parse_from(["winterd", "run", "--no-launch"]).unwrap_err();
        assert!(!is_informational(run_style.kind()));
    }

    #[test]
    fn repeated_or_conflicting_flags_are_rejected() {
        let both = DaemonCli::try_parse_from(["winterd", "--launch", "--no-launch"]).unwrap();
        assert!(both.launch_mode().is_err());
        let repeated_force =
            DaemonCli::try_parse_from(["winterd", "--launch", "--launch"]).unwrap();
        assert!(repeated_force.launch_mode().is_err());
        let repeated_suppress =
            DaemonCli::try_parse_from(["winterd", "--no-launch", "--no-launch"]).unwrap();
        assert!(repeated_suppress.launch_mode().is_err());
    }

    #[test]
    fn help_and_version_exit_successfully_as_informational_requests() {
        let help = DaemonCli::try_parse_from(["winterd", "--help"]).unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(is_informational(help.kind()));
        let version = DaemonCli::try_parse_from(["winterd", "-V"]).unwrap_err();
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(is_informational(version.kind()));
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
