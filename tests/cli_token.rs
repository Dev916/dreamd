//! API token file: `dreamd token` and the doctor `api token` check.

use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use tempfile::TempDir;

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

fn stdout(cmd: &mut Command, code: i32) -> String {
    let out = cmd.assert().code(code);
    String::from_utf8(out.get_output().stdout.clone()).unwrap()
}

fn token_line(out: &str) -> String {
    out.lines()
        .find(|l| l.contains("api token"))
        .unwrap_or_else(|| panic!("no `api token` line in:\n{out}"))
        .to_string()
}

#[test]
fn token_is_created_0600_and_reused() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("data/api-token");
    assert!(!file.exists());
    let first = stdout(dreamd(&tmp).arg("token"), 0);
    let tok = first.trim();
    assert_eq!(tok.len(), 64, "{first:?}");
    assert!(tok.chars().all(|c| c.is_ascii_hexdigit()), "{tok}");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), tok);
    let second = stdout(dreamd(&tmp).arg("token"), 0);
    assert_eq!(second.trim(), tok);
}

#[test]
fn doctor_api_token_ok_absent_ok_0600_fail_wider() {
    let tmp = TempDir::new().unwrap();
    // Absent: ok (created on first serve).
    let line = token_line(&stdout(dreamd(&tmp).arg("doctor"), 0));
    assert!(line.starts_with("ok"), "{line}");
    // Present, 0600: ok with path and perms.
    stdout(dreamd(&tmp).arg("token"), 0);
    let file = tmp.path().join("data/api-token");
    let line = token_line(&stdout(dreamd(&tmp).arg("doctor"), 0));
    assert!(line.starts_with("ok"), "{line}");
    assert!(line.contains(&file.display().to_string()), "{line}");
    assert!(line.contains("0600"), "{line}");
    // Wider: FAIL.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let line = token_line(&stdout(dreamd(&tmp).arg("doctor"), 1));
    assert!(line.starts_with("FAIL"), "{line}");
    assert!(line.contains("0644"), "{line}");
}
