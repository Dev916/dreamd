//! Task 2 acceptance: list / show against a fresh data dir.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn dreamd(tmp: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("dreamd").unwrap();
    cmd.env("DREAMD_CONFIG", tmp.path().join("no-such-config.toml"))
        .env("DREAMD_DATA_DIR", tmp.path().join("data"))
        .env("DREAMD_CONFIG_DIRS", tmp.path().join("claude-cfg"))
        .env("DREAMD_CLAUDE_BIN", "/bin/sh");
    cmd
}

#[test]
fn list_on_fresh_data_dir_prints_no_dreams_and_creates_db() {
    let tmp = TempDir::new().unwrap();
    dreamd(&tmp)
        .arg("list")
        .assert()
        .code(0)
        .stdout("no dreams\n");
    assert!(tmp.path().join("data/dreamd.db").is_file());
}

#[test]
fn list_json_prints_empty_array() {
    let tmp = TempDir::new().unwrap();
    let out = dreamd(&tmp).args(["list", "--json"]).assert().code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout.trim(), "[]");
}

#[test]
fn show_unknown_dream_exits_3() {
    let tmp = TempDir::new().unwrap();
    dreamd(&tmp)
        .args(["show", "drm_nonexistent"])
        .assert()
        .code(3)
        .stdout("")
        .stderr(predicate::str::contains("dream not found: drm_nonexistent"));
}
