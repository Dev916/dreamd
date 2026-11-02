//! `dreamd apply` / `rollback`: Task 9 acceptance criteria, end to end
//! against a completed stub dream.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use predicates::prelude::*;

/// Sorted (relative path, bytes) of every file under `dir` — equal trees
/// hash equal under `fsops::hash_dir`.
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

/// Run a completed stub dream; return (id, dream dir).
fn completed_dream(f: &common::Fixture) -> (String, PathBuf) {
    let out = common::cmd(f)
        .env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .args(["dream", common::SLUG, "--force"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let i = stdout.find("drm_").expect("dream id");
    let id = stdout[i..].split_whitespace().next().unwrap().to_string();
    let dir = f.data_dir.join("dreams").join(&id);
    (id, dir)
}

fn no_siblings(f: &common::Fixture) {
    for suffix in ["dreamd-new", "dreamd-old"] {
        let p = f.memory_dir.with_file_name(format!("memory.{suffix}"));
        assert!(!p.exists(), "{} left behind", p.display());
    }
}

#[test]
fn apply_swaps_in_out_and_records_it() {
    let f = common::fixture();
    let (id, dir) = completed_dream(&f);
    let original = tree(&f.memory_dir);
    assert_ne!(original, tree(&dir.join("out")), "stub dream changed out/");
    let backup = dir.join("backup");
    common::cmd(&f)
        .args(["apply", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains(format!(
            "applied {id}: backup at {}",
            backup.display()
        )));
    assert_eq!(tree(&f.memory_dir), tree(&dir.join("out")));
    assert_eq!(tree(&backup), original);
    no_siblings(&f);
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("status       applied"));
    let scan = common::cmd(&f)
        .arg("scan")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let scan = String::from_utf8(scan.stdout).unwrap();
    assert!(scan.contains(common::SLUG), "{scan}");
    assert!(!scan.contains("last=never"), "{scan}");
    let last = scan.split("last=").nth(1).unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(last.split_whitespace().next().unwrap()).is_ok(),
        "{scan}"
    );
}

#[test]
fn apply_refuses_when_memory_changed_then_force_applies() {
    let f = common::fixture();
    let (id, dir) = completed_dream(&f);
    std::fs::write(f.memory_dir.join("fact-a.md"), "edited by someone\n").unwrap();
    let edited = tree(&f.memory_dir);
    common::cmd(&f)
        .args(["apply", &id])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(
            "refused: memory changed since snapshot (rerun the dream or use --force)",
        ));
    assert_eq!(tree(&f.memory_dir), edited, "live dir touched");
    assert!(!dir.join("backup").exists());
    common::cmd(&f)
        .args(["apply", &id, "--force"])
        .assert()
        .code(0);
    assert_eq!(tree(&f.memory_dir), tree(&dir.join("out")));
    assert_eq!(tree(&dir.join("backup")), edited);
}

#[test]
fn apply_refuses_failed_and_applied_dreams() {
    let f = common::fixture();
    let (id, _) = completed_dream(&f);
    common::cmd(&f).args(["apply", &id]).assert().code(0);
    common::cmd(&f)
        .args(["apply", &id])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused: dream is applied"));

    let g = common::fixture();
    let out = common::cmd(&g)
        .env("DREAMD_STUB_MODE", "auto")
        .env("DREAMD_STUB_ESCAPE", "../in/evil.md")
        .timeout(Duration::from_secs(60))
        .args(["dream", common::SLUG, "--force"])
        .assert()
        .code(4)
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    let i = stderr.find("drm_").expect("dream id");
    let failed = stderr[i..].split_whitespace().next().unwrap().to_string();
    let before = tree(&g.memory_dir);
    common::cmd(&g)
        .args(["apply", &failed])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused: dream is failed"));
    assert_eq!(tree(&g.memory_dir), before);
    common::cmd(&g)
        .args(["apply", "drm_nosuchdream"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("dream not found"));
}

#[test]
fn rollback_restores_the_snapshot() {
    let f = common::fixture();
    let (id, dir) = completed_dream(&f);
    common::cmd(&f).args(["apply", &id]).assert().code(0);
    common::cmd(&f)
        .args(["rollback", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains(format!("rolled back {id}")));
    assert_eq!(tree(&f.memory_dir), tree(&dir.join("in")));
    no_siblings(&f);
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("status       rolled_back"));
    common::cmd(&f)
        .args(["rollback", &id])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused: dream is rolled_back"));
}

#[test]
fn failure_after_backup_leaves_live_dir_original() {
    let f = common::fixture();
    let (id, dir) = completed_dream(&f);
    common::cmd(&f)
        .args(["apply", &id])
        .env("DREAMD_FAIL_AFTER_BACKUP", "1")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("apply failed"));
    assert_eq!(tree(&f.memory_dir), tree(&dir.join("in")));
    no_siblings(&f);
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("status       completed"));
    // Still appliable once the fault is gone.
    common::cmd(&f).args(["apply", &id]).assert().code(0);
    assert_eq!(tree(&f.memory_dir), tree(&dir.join("out")));
}
