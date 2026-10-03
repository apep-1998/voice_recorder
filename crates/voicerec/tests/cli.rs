//! Integration tests for the `voicerec` CLI.

use assert_cmd::Command;
use predicates::prelude::*;

fn voicerec() -> Command {
    Command::cargo_bin("voicerec").expect("binary builds")
}

#[test]
fn help_lists_config_subcommand() {
    voicerec()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("config"));
}

#[test]
fn config_check_accepts_valid_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, recorder_core::config::DEFAULT_CONFIG_TOML).unwrap();
    voicerec()
        .args(["--config", path.to_str().unwrap(), "config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config OK"));
}

#[test]
fn config_check_rejects_invalid_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[storage]\nretention = \"1s\"\n").unwrap();
    voicerec()
        .args(["--config", path.to_str().unwrap(), "config", "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("retention"));
}

#[test]
fn config_check_rejects_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[storage]\nbogus_key = 1\n").unwrap();
    voicerec()
        .args(["--config", path.to_str().unwrap(), "config", "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bogus_key"));
}

#[test]
fn config_path_prints_explicit_path() {
    voicerec()
        .args(["--config", "/some/where/config.toml", "config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/some/where/config.toml"));
}
