//! Shared command-line implementation for `winter.exe` and `winterminalp.exe`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;

use crate::integration::{DoctorReport, IntegrationConfig, doctor, install, plan, uninstall};
use crate::platform::windows::relaunch_current_process_elevated;
use crate::{
    AppError, AppResult, ControllerConfig, ControllerOptions, bridge_is_ready,
    config::default_config_path, dashboard::dashboard_path, run_controller, ui,
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "tmux-style keyboard control for the native Windows Terminal"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Show the full configuration, or locate or edit the user configuration file.
    Config {
        /// Print only the configuration file path.
        #[arg(long = "path", conflicts_with = "edit")]
        path_only: bool,
        /// Open the configuration file in Notepad.
        #[arg(long)]
        edit: bool,
    },
    /// Show the settings changes that install would make.
    ///
    /// Exit status: 0 ready, 2 not installable, 1 failure.
    Plan,
    /// Install the managed action fragment and hidden bridge keybindings.
    Install,
    /// Remove only integration entries still owned by Winter.
    Uninstall,
    /// Diagnose the current Windows Terminal integration.
    ///
    /// Exit status: 0 healthy, 2 needs attention, 1 failure.
    Doctor,
    /// Run the keyboard controller in this process.
    Run {
        /// Do not open a new native Windows Terminal window.
        #[arg(long)]
        no_launch: bool,
    },
    /// Start a background controller and open native Windows Terminal.
    Launch,
    /// Show a live dashboard of panes, focus, and prefix state in this terminal
    Ui {
        #[arg(long)]
        once: bool,
        /// State file override (testing)
        #[arg(long, hide = true)]
        path: Option<PathBuf>,
    },
}

/// Parses `args` using `bin_name` for usage text and runs the selected command.
pub fn run(bin_name: &'static str) -> ExitCode {
    let matches = Cli::command().name(bin_name).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    match execute(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> AppResult<ExitCode> {
    let command = cli.command.unwrap_or(CliCommand::Launch);
    if let CliCommand::Config { path_only, edit } = command {
        return configure(path_only, edit);
    }
    if let CliCommand::Ui { once, path } = &command {
        let path = match path {
            Some(path) => path.clone(),
            None => dashboard_path()?,
        };
        return ui::run(ui::UiOptions { path, once: *once });
    }

    let integration = IntegrationConfig::from_environment()?;
    match command {
        CliCommand::Plan => {
            let report = plan(&integration)?;
            print_json(&report)?;
            Ok(if report.can_install {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        CliCommand::Install => {
            let report = install(&integration)?;
            print_json(&report)?;
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Uninstall => {
            let report = uninstall(&integration)?;
            print_json(&report)?;
            Ok(ExitCode::SUCCESS)
        }
        CliCommand::Doctor => {
            let report = build_doctor_report(&integration, &default_config_path()?)?;
            print_json(&report)?;
            Ok(ExitCode::from(doctor_exit_code(report.healthy)))
        }
        CliCommand::Run { no_launch } => run_command(&integration, no_launch),
        CliCommand::Launch => launch(&integration),
        CliCommand::Config { .. } => unreachable!("config is handled before integration is loaded"),
        CliCommand::Ui { .. } => unreachable!("ui is handled before integration is loaded"),
    }
}

/// Single JSON document written to stdout by `winter doctor`.
#[derive(Debug, Serialize)]
struct DoctorCommandReport {
    schema_version: u32,
    healthy: bool,
    config: ConfigDiagnosis,
    integration: DoctorReport,
}

/// Configuration half of the doctor envelope.
#[derive(Debug, Serialize)]
struct ConfigDiagnosis {
    path: String,
    ok: bool,
    error: Option<String>,
}

/// Diagnoses the configuration file and the Windows Terminal integration.
///
/// An invalid configuration is reported instead of aborting, so `winter
/// doctor` always produces a full report; only environment and serialization
/// failures abort with a hard error (exit status 1).
fn build_doctor_report(
    integration: &IntegrationConfig,
    config_path: &Path,
) -> AppResult<DoctorCommandReport> {
    let config = diagnose_config(config_path);
    let integration_report = doctor(integration)?;
    let healthy = config.ok && bridge_is_ready(&integration_report);
    Ok(DoctorCommandReport {
        schema_version: 1,
        healthy,
        config,
        integration: integration_report,
    })
}

/// Inspects the configuration file without aborting on invalid content.
///
/// A missing file counts as healthy because the controller runs on defaults
/// until the user creates one. When the file parses, its key chords are also
/// compiled here so a broken shortcut is reported by `doctor` as well.
fn diagnose_config(path: &Path) -> ConfigDiagnosis {
    let rendered = path.display().to_string();
    if !path.exists() {
        return ConfigDiagnosis {
            path: rendered,
            ok: true,
            error: None,
        };
    }
    let error = match ControllerConfig::load(path) {
        Ok(config) => config.prefix_config().err(),
        Err(error) => Some(error),
    };
    ConfigDiagnosis {
        ok: error.is_none(),
        path: rendered,
        error: error.map(|error| error.to_string()),
    }
}

/// Maps the doctor verdict onto the documented exit statuses (0 or 2).
const fn doctor_exit_code(healthy: bool) -> u8 {
    if healthy { 0 } else { 2 }
}

fn run_command(integration: &IntegrationConfig, no_launch: bool) -> AppResult<ExitCode> {
    let arguments = if no_launch {
        ["run", "--no-launch"].as_slice()
    } else {
        ["run"].as_slice()
    };
    if relaunch_controller_elevated(arguments)? {
        return Ok(ExitCode::SUCCESS);
    }
    ensure_bridge_ready(integration)?;
    let config = ControllerConfig::load_or_create(&default_config_path()?)?;
    let controller_report = run_controller(
        &config,
        ControllerOptions {
            launch_terminal: config.launch_terminal_on_start && !no_launch,
            bridge_ready: true,
            ..ControllerOptions::default()
        },
    )?;
    print_json(&controller_report)?;
    Ok(ExitCode::SUCCESS)
}

/// Installs the Windows Terminal action bridge on first use.
///
/// This keeps `winter` a single command: a fresh clone only needs the one-time
/// `install.ps1` to put the binary on `PATH`, and every later launch repairs a
/// missing integration automatically. A genuine conflict still fails loudly
/// instead of silently overwriting the user's settings.
fn ensure_bridge_ready(integration: &IntegrationConfig) -> AppResult<()> {
    if bridge_is_ready(&doctor(integration)?) {
        return Ok(());
    }
    eprintln!("Windows Terminal integration is not installed; setting it up now...");
    install(integration)?;
    if bridge_is_ready(&doctor(integration)?) {
        Ok(())
    } else {
        Err(AppError::InvalidConfiguration(
            "Windows Terminal integration could not be installed automatically; run `winter doctor` for details"
                .to_owned(),
        ))
    }
}

fn configure(path_only: bool, edit: bool) -> AppResult<ExitCode> {
    let path = default_config_path()?;
    if path_only {
        println!("{}", path.display());
    } else if edit {
        if !path.exists() {
            let _config = ControllerConfig::load_or_create(&path)?;
        }
        let _editor = Command::new("notepad.exe")
            .arg(&path)
            .spawn()
            .map_err(|error| AppError::io("open controller config editor", &path, error))?;
        eprintln!(
            "Opened {}. Restart the controller after saving.",
            path.display()
        );
    } else {
        let config = ControllerConfig::load_or_create(&path)?;
        println!("# path = {}", path.display());
        print!("{}", config.to_pretty_toml()?);
    }
    Ok(ExitCode::SUCCESS)
}

fn launch(integration: &IntegrationConfig) -> AppResult<ExitCode> {
    if relaunch_controller_elevated(&["launch"])? {
        return Ok(ExitCode::SUCCESS);
    }
    let _config = ControllerConfig::load_or_create(&default_config_path()?)?;
    ensure_bridge_ready(integration)?;

    spawn_background_controller()?;
    eprintln!("Winter elevated controller is starting; Windows Terminal will open elevated.");
    Ok(ExitCode::SUCCESS)
}

/// Resolves which program runs the background controller for `current_exe`.
///
/// Returns the program, its arguments, and a warning to print when the
/// dedicated `winterd.exe` daemon is missing and diagnostics (the
/// last-error.log it writes) become unavailable.
fn background_launch_plan(current_exe: &Path) -> (PathBuf, Vec<&'static str>, Option<String>) {
    let sibling_daemon = current_exe.with_file_name("winterd.exe");
    if sibling_daemon.is_file() {
        (sibling_daemon, vec!["--launch"], None)
    } else {
        (
            current_exe.to_path_buf(),
            vec!["run", "--no-launch"],
            Some(format!(
                "warning: winterd.exe was not found next to {}; falling back to `run --no-launch`, so diagnostics (last-error.log) will be unavailable",
                current_exe.display()
            )),
        )
    }
}

fn spawn_background_controller() -> AppResult<()> {
    let current_exe = env::current_exe()
        .map_err(|error| AppError::io("resolve current executable", PathBuf::from("."), error))?;
    let (program, arguments, warning) = background_launch_plan(&current_exe);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }

    let mut command = Command::new(&program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _controller = command
        .spawn()
        .map_err(|error| AppError::io("start background controller", program, error))?;
    Ok(())
}

fn relaunch_controller_elevated(arguments: &[&str]) -> AppResult<bool> {
    relaunch_current_process_elevated(arguments).map_err(AppError::platform)
}

fn print_json(value: &impl Serialize) -> AppResult<()> {
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        AppError::InvalidConfiguration(format!("serialize command report: {error}"))
    })?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn ensure_bridge_ready_installs_then_is_idempotent() {
        let temp = tempdir().expect("temporary directory should be created");
        let settings = temp
            .path()
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(settings.parent().expect("settings should have a parent"))
            .expect("fixture directory should be created");
        fs::write(&settings, b"{}\n").expect("fixture settings should be written");

        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        ensure_bridge_ready(&config).expect("first launch should install the bridge");
        assert!(bridge_is_ready(
            &doctor(&config).expect("doctor should succeed")
        ));

        ensure_bridge_ready(&config).expect("second launch should be a no-op");
    }

    #[test]
    fn doctor_reports_broken_config_without_aborting() {
        let temp = tempdir().expect("temporary directory should be created");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "prefix = \nthis is not valid toml [[[\n")
            .expect("garbage configuration should be written");
        let integration = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = build_doctor_report(&integration, &config_path)
            .expect("an invalid configuration must not abort the doctor report");
        let value = serde_json::to_value(&report).expect("doctor report must serialize to JSON");

        assert_eq!(value["schema_version"], serde_json::json!(1));
        assert_eq!(value["config"]["ok"], serde_json::Value::Bool(false));
        assert!(
            value["config"]["error"].is_string(),
            "a broken config must carry an error message: {value}"
        );
        let rendered_path = config_path.display().to_string();
        assert_eq!(
            value["config"]["path"].as_str(),
            Some(rendered_path.as_str())
        );
        assert!(
            value["integration"].is_object(),
            "the integration report must stay present: {value}"
        );
        assert_eq!(value["healthy"], serde_json::Value::Bool(false));
        assert_eq!(doctor_exit_code(report.healthy), 2);
    }

    #[test]
    fn doctor_reports_valid_configuration_as_ok() {
        let temp = tempdir().expect("temporary directory should be created");
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            "schema_version = 2\nprefix_timeout_ms = 1500\nlaunch_terminal_on_start = true\n",
        )
        .expect("valid configuration should be written");
        let integration = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = build_doctor_report(&integration, &config_path)
            .expect("a valid configuration must produce a report");
        let value = serde_json::to_value(&report).expect("doctor report must serialize to JSON");

        assert_eq!(value["config"]["ok"], serde_json::Value::Bool(true));
        assert!(
            value["config"]["error"].is_null(),
            "a healthy config must not carry an error: {value}"
        );
        assert!(value["integration"].is_object());
        assert_eq!(
            doctor_exit_code(report.healthy),
            2,
            "the bridge is not installed in the temporary environment"
        );
    }

    #[test]
    fn doctor_flags_chords_that_load_but_do_not_compile() {
        let temp = tempdir().expect("temporary directory should be created");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "schema_version = 2\nprefix = \"b\"\n")
            .expect("configuration fixture should be written");
        let integration = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = build_doctor_report(&integration, &config_path)
            .expect("a bad chord must not abort the doctor report");
        let value = serde_json::to_value(&report).expect("doctor report must serialize to JSON");

        assert_eq!(value["config"]["ok"], serde_json::Value::Bool(false));
        assert_eq!(value["healthy"], serde_json::Value::Bool(false));
        assert_eq!(doctor_exit_code(report.healthy), 2);
    }

    #[test]
    fn doctor_accepts_an_absent_configuration_file() {
        let temp = tempdir().expect("temporary directory should be created");
        let config_path = temp.path().join("config.toml");
        let integration = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );

        let report = build_doctor_report(&integration, &config_path)
            .expect("an absent configuration must produce a report");
        let value = serde_json::to_value(&report).expect("doctor report must serialize to JSON");

        assert_eq!(value["config"]["ok"], serde_json::Value::Bool(true));
        assert!(value["config"]["error"].is_null());
    }

    #[test]
    fn background_launch_prefers_the_daemon_and_warns_on_the_fallback() {
        let temp = tempdir().expect("temporary directory should be created");
        let exe = temp.path().join("winter.exe");

        let (program, arguments, warning) = background_launch_plan(&exe);
        assert_eq!(program, exe, "without a daemon the current exe must run");
        assert_eq!(arguments, ["run", "--no-launch"]);
        let warning = warning.expect("a missing winterd.exe must warn");
        assert!(
            warning.contains("warning: winterd.exe was not found"),
            "unexpected warning: {warning}"
        );
        assert!(
            warning.contains("last-error.log"),
            "the warning must explain the lost diagnostics: {warning}"
        );

        let daemon = temp.path().join("winterd.exe");
        fs::write(&daemon, b"").expect("daemon fixture should be written");
        let (program, arguments, warning) = background_launch_plan(&exe);
        assert_eq!(program, daemon, "the dedicated daemon must be preferred");
        assert_eq!(arguments, ["--launch"]);
        assert!(warning.is_none(), "the daemon path must not warn");
    }
}
