//! Contract tests for the `winter` CLI.
//!
//! Covers the documented report contract of `winter doctor`:
//! `{ schema_version, healthy, config: { path, ok, error }, integration }`,
//! with exit codes `0` = healthy, `2` = needs attention, `1` = hard failure.
//! Every test redirects `LOCALAPPDATA` to a fresh temporary directory so the
//! real user configuration and Windows Terminal settings are never touched;
//! no test here requires a running Windows Terminal.

use std::fs;

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
    let config_dir = temp.path().join("WinTerminalP");
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
