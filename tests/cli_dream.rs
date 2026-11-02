//! `dreamd dream` / `show` / `diff` / `summary`: Task 8 acceptance criteria,
//! run end to end against `tests/stub-claude.sh` in `auto` mode.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use predicates::prelude::*;

fn dream(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = common::cmd(f);
    c.env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .arg("dream")
        .arg(common::SLUG);
    c
}

/// sha256-free tree fingerprint: sorted (relative path, bytes).
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v: Vec<(PathBuf, Vec<u8>)> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            (
                e.path().strip_prefix(dir).unwrap().to_path_buf(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// The single dream dir under `<data_dir>/dreams/`.
fn only_dream_dir(f: &common::Fixture) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(f.data_dir.join("dreams"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn dream_id(stdout: &str) -> String {
    let i = stdout.find("drm_").expect("dream id in stdout");
    stdout[i..].split_whitespace().next().unwrap().to_string()
}

#[test]
fn forced_dream_completes_and_leaves_live_memory_untouched() {
    let f = common::fixture();
    let before = tree(&f.memory_dir);
    let out = dream(&f)
        .arg("--force")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("completed: 3 sessions mined, +1 -0 ~1 files"),
        "{stdout}"
    );
    let id = dream_id(&stdout);
    let dir = only_dream_dir(&f);
    assert_eq!(dir.file_name().unwrap().to_str().unwrap(), id);
    assert!(
        stdout.contains(&dir.join("out").display().to_string()),
        "{stdout}"
    );
    for p in ["in", "out", "findings"] {
        assert!(dir.join(p).is_dir(), "{p} missing");
    }
    for p in ["aggregate.md", "dream.diff"] {
        assert!(dir.join(p).is_file(), "{p} missing");
    }
    assert_eq!(std::fs::read_dir(dir.join("findings")).unwrap().count(), 3);
    assert_eq!(tree(&f.memory_dir), before, "live memory dir changed");

    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("completed"))
        .stdout(predicate::str::contains("opus"))
        .stdout(predicate::str::contains("sessions     3"))
        .stdout(predicate::str::contains("cost_usd=0.0040"))
        .stdout(predicate::str::contains("Merged 1 insight."));
    let diff = common::cmd(&f)
        .args(["diff", &id])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(diff)
            .unwrap()
            .starts_with("--- a/MEMORY.md"),
        "diff header"
    );
    let summary = common::cmd(&f)
        .args(["summary", &id])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(summary, std::fs::read(dir.join("aggregate.md")).unwrap());
}

#[test]
fn gate_skip_exits_2_with_reason() {
    let f = common::fixture();
    std::fs::write(f.config_file(), "min_sessions = 10\n").unwrap();
    dream(&f)
        .assert()
        .code(2)
        .stdout(predicate::str::contains("need 10 sessions, have 3"));
    assert!(!f.data_dir.join("dreams").exists());
}

#[test]
fn unknown_project_exits_3() {
    let f = common::fixture();
    common::cmd(&f)
        .args(["dream", "nosuchslug", "--force"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("project not found"));
}

#[test]
fn escaped_write_fails_the_dream_and_keeps_out() {
    let f = common::fixture();
    let before = tree(&f.memory_dir);
    // The aggregator runs with cwd = <dream_dir>/out.
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_ESCAPE", "../in/evil.md")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("violation"));
    let dir = only_dream_dir(&f);
    assert!(dir.join("in/evil.md").is_file());
    assert!(dir.join("out/dreamed.md").is_file(), "out/ kept");
    assert_eq!(tree(&f.memory_dir), before);
    let id = dir.file_name().unwrap().to_str().unwrap().to_string();
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("failed"))
        .stdout(predicate::str::contains("violation"));
}

#[test]
fn instructions_reach_the_aggregator_prompt() {
    let f = common::fixture();
    let prompt_out = f.root.path().join("prompt.txt");
    dream(&f)
        .args(["--force", "--instructions", "focus on X"])
        .env("DREAMD_STUB_PROMPT_OUT", &prompt_out)
        .assert()
        .code(0);
    let p = std::fs::read_to_string(&prompt_out).unwrap();
    assert!(p.contains("## Operator instructions"), "{p}");
    assert!(p.contains("focus on X"), "{p}");
}

#[test]
fn cancel_refuses_terminal_dream() {
    let f = common::fixture();
    let out = dream(&f)
        .arg("--force")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let id = dream_id(&String::from_utf8(out.stdout).unwrap());
    common::cmd(&f)
        .args(["cancel", &id])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused: dream is completed"));
}
