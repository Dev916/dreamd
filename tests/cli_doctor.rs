//! Task 1 acceptance: --version, doctor, config show.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A hermetic `dreamd` command: config file absent, config dirs and data dir
/// inside a tempdir, claude bin pointed at an executable that exists.
fn dreamd(tmp: &TempDir) -> Command {
    let cfg_dir = tmp.path().join("claude-cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let mut cmd = Command::cargo_bin("dreamd").unwrap();
    cmd.env("DREAMD_CONFIG", tmp.path().join("no-such-config.toml"))
        .env("DREAMD_DATA_DIR", tmp.path().join("data"))
        .env("DREAMD_CONFIG_DIRS", &cfg_dir)
        .env("DREAMD_CLAUDE_BIN", "/bin/sh");
    cmd
}

#[test]
fn version_prints_name_and_version() {
    Command::cargo_bin("dreamd")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("dreamd 0.1.0"));
}

#[test]
fn doctor_all_ok() {
    let tmp = TempDir::new().unwrap();
    let out = dreamd(&tmp).arg("doctor").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    for name in ["claude binary", "config dirs", "data dir", "database"] {
        let line = stdout
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("no `{name}` line in:\n{stdout}"));
        assert!(line.starts_with("ok"), "expected ok: {line}");
    }
    assert!(tmp.path().join("data/dreamd.db").exists());
}

#[test]
fn doctor_fails_on_missing_claude_bin() {
    let tmp = TempDir::new().unwrap();
    let out = dreamd(&tmp)
        .env("DREAMD_CLAUDE_BIN", "/nonexistent")
        .arg("doctor")
        .assert()
        .code(1);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let line = stdout
        .lines()
        .find(|l| l.contains("claude binary"))
        .expect("claude binary line");
    assert!(line.starts_with("FAIL"), "expected FAIL: {line}");
    let db = stdout.lines().find(|l| l.contains("database")).unwrap();
    assert!(db.starts_with("ok"), "other checks still run: {db}");
}

#[test]
fn doctor_finds_bare_name_on_path() {
    let tmp = TempDir::new().unwrap();
    dreamd(&tmp)
        .env("DREAMD_CLAUDE_BIN", "sh")
        .arg("doctor")
        .assert()
        .success();
    dreamd(&tmp)
        .env("DREAMD_CLAUDE_BIN", "definitely-not-a-real-binary-xyz")
        .arg("doctor")
        .assert()
        .code(1);
}

#[test]
fn doctor_json_output() {
    let tmp = TempDir::new().unwrap();
    let out = dreamd(&tmp).args(["doctor", "--json"]).assert().success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 4);
    assert!(arr.iter().all(|c| c["ok"] == true));
}

#[test]
fn config_show_reflects_env() {
    let tmp = TempDir::new().unwrap();
    let out = dreamd(&tmp)
        .env("DREAMD_CONFIG_DIRS", "/tmp/a:/tmp/b")
        .args(["config", "show"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: toml::Value = toml::from_str(&stdout).expect("stdout is TOML");
    assert_eq!(
        v["config_dirs"],
        toml::Value::Array(vec!["/tmp/a".into(), "/tmp/b".into()])
    );
    assert_eq!(v["claude_bin"].as_str(), Some("/bin/sh"));
    assert_eq!(v["min_hours"].as_integer(), Some(24));
    assert_eq!(v["worker"]["model"].as_str(), Some("sonnet"));
    assert_eq!(v["schedule"]["minute"].as_integer(), Some(30));
}

#[test]
fn unimplemented_subcommand_exits_1() {
    let tmp = TempDir::new().unwrap();
    dreamd(&tmp)
        .args(["diff", "drm_x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not implemented"));
}
