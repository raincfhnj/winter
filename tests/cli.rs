//! Contract tests for the `winter` CLI.
//!
//! Covers the documented report contract of `winter doctor`:
//! `{ schema_version, healthy, config: { path, ok, error }, integration }`,
//! with exit codes `0` = healthy, `2` = needs attention, `1` = hard failure;
//! the documented `winter plan` contract (`0` ready, `2` not installable,
//! stdout a single JSON plan); plus `winter config --path` and
//! `winter --version`. Every test that touches configuration redirects
//! `LOCALAPPDATA` to a fresh temporary directory so the real user
//! configuration and Windows Terminal settings are never touched; no test
//! here requires a running Windows Terminal.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::tempdir;

#[test]
fn help_exits_zero_and_documents_doctor() {
    Command::cargo_bin("winter")
        .expect("winter binary should build")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn doctor_reports_broken_config_as_needing_attention() {
    let temp = tempdir().expect("temporary directory should be created");
    let config_dir = temp.path().join("Winter");
    fs::create_dir_all(&config_dir).expect("config directory should be created");
    fs::write(
        config_dir.join("config.toml"),
        "prefix = \nthis is not valid toml [[[\n",
    )
    .expect("garbage configuration should be written");

    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .env("LOCALAPPDATA", temp.path())
        .arg("doctor")
        .assert()
        .code(2);

    let report: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("doctor stdout should be JSON");
    assert_eq!(
        report["config"]["ok"],
        Value::Bool(false),
        "broken config must be reported as unhealthy: {report}"
    );
    assert!(
        report["integration"].is_object(),
        "doctor must include an integration object: {report}"
    );
    assert_eq!(
        report["healthy"],
        Value::Bool(false),
        "a broken config cannot be healthy: {report}"
    );
}

#[test]
fn doctor_accepts_a_fresh_config_directory() {
    let temp = tempdir().expect("temporary directory should be created");

    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .env("LOCALAPPDATA", temp.path())
        .arg("doctor")
        .assert();

    let output = assert.get_output();
    let code = output.status.code();
    assert!(
        matches!(code, Some(0) | Some(2)),
        "a valid config must not be a hard failure (exit 1); got {code:?}"
    );

    let report: Value =
        serde_json::from_slice(&output.stdout).expect("doctor stdout should be JSON");
    assert_eq!(
        report["config"]["ok"],
        Value::Bool(true),
        "absent config in a fresh directory must be healthy: {report}"
    );
    assert!(
        report["integration"].is_object(),
        "doctor must include an integration object: {report}"
    );
}

#[test]
fn plan_in_a_fresh_environment_reports_not_installable_json() {
    let temp = tempdir().expect("temporary directory should be created");

    // Documented contract (`winter plan --help`): 0 ready, 2 not installable,
    // 1 failure. Target discovery (src/integration/discovery.rs) only probes
    // under LOCALAPPDATA, so an empty redirect can never yield a channel and
    // the real outcome here is exit 2 with a JSON plan on stdout.
    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .env("LOCALAPPDATA", temp.path())
        .arg("plan")
        .assert()
        .code(2);

    let report: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("plan stdout should be JSON");
    assert_eq!(
        report["schemaVersion"].as_u64(),
        Some(1),
        "plan must declare the integration schema version: {report}"
    );
    assert_eq!(
        report["canInstall"],
        Value::Bool(false),
        "a fresh environment without channels cannot install: {report}"
    );
    assert!(
        report["targets"].as_array().is_some_and(Vec::is_empty),
        "no channel can be discovered under a fresh LOCALAPPDATA: {report}"
    );
    assert!(
        report["issues"].as_array().is_some_and(|issues| {
            issues.iter().any(|issue| {
                issue
                    .as_str()
                    .is_some_and(|text| text.contains("no initialized Windows Terminal"))
            })
        }),
        "the missing-channel reason must be reported: {report}"
    );
}

#[test]
fn config_path_prints_a_file_under_the_redirected_local_app_data() {
    let temp = tempdir().expect("temporary directory should be created");

    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .env("LOCALAPPDATA", temp.path())
        .args(["config", "--path"])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let printed = Path::new(stdout.trim());
    assert!(
        printed.starts_with(temp.path()),
        "--path must resolve inside the redirected LOCALAPPDATA: {printed:?}"
    );
    assert_eq!(
        printed.file_name(),
        Some(OsStr::new("config.toml")),
        "the printed path must be the configuration file: {printed:?}"
    );
    assert_eq!(
        printed.parent().and_then(Path::file_name),
        Some(OsStr::new("Winter")),
        "the configuration file must live in the Winter directory: {printed:?}"
    );
}

#[test]
fn version_exits_zero_and_prints_the_cargo_version() {
    Command::cargo_bin("winter")
        .expect("winter binary should build")
        .arg("--version")
        .assert()
        .success()
        // `env!("CARGO_PKG_VERSION")` is read from the same Cargo.toml the
        // clap `version` attribute embeds in the binary.
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_documents_the_ui_subcommand() {
    Command::cargo_bin("winter")
        .expect("winter binary should build")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("ui"))
        .stdout(predicate::str::contains("live dashboard"));
}

#[test]
fn ui_once_renders_a_fixture_dashboard() {
    let temp = tempdir().expect("temporary directory should be created");
    let fixture = temp.path().join("dashboard.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock must be past the Unix epoch")
        .as_millis() as u64;
    let state = serde_json::json!({
        "schemaVersion": 1,
        "updatedUnixMs": now,
        "controllerRunning": true,
        "prefixArmed": true,
        "mouseResizeEnabled": false,
        "terminalPresent": true,
        "panes": [
            { "x": 0, "y": 0, "width": 40, "height": 12, "focused": true },
            { "x": 40, "y": 0, "width": 40, "height": 12, "focused": false },
            { "x": 0, "y": 12, "width": 80, "height": 12, "focused": false }
        ],
        "dividers": []
    });
    fs::write(
        &fixture,
        serde_json::to_vec(&state).expect("fixture dashboard should serialize"),
    )
    .expect("fixture dashboard should be written");

    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .args(["ui", "--once", "--path"])
        .arg(&fixture)
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("winter ui"), "missing header: {stdout}");
    assert!(stdout.contains("3 panes"), "missing pane count: {stdout}");
    assert!(
        stdout.contains("prefix: ARMED"),
        "missing prefix state: {stdout}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "--once must not emit ANSI sequences: {stdout:?}"
    );
}

#[test]
fn ui_once_reports_offline_for_a_missing_state_file() {
    let temp = tempdir().expect("temporary directory should be created");
    let missing = temp.path().join("dashboard.json");

    let assert = Command::cargo_bin("winter")
        .expect("winter binary should build")
        .args(["ui", "--once", "--path"])
        .arg(&missing)
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("CONTROLLER OFFLINE"),
        "a missing state file must render the offline banner: {stdout}"
    );
    assert!(stdout.contains("winter ui"), "missing header: {stdout}");
}
