//! `dreamd schedule install|uninstall|status` and `dream --all` (+ auto_apply):
//! Task 11 acceptance criteria. launchctl is never invoked (DREAMD_SKIP_LAUNCHCTL=1)
//! and HOME is a temp dir, so the real LaunchAgents are untouched.

mod common;

use std::path::Path;
use std::time::Duration;

use predicates::prelude::*;
use tempfile::TempDir;

fn sched(f: &common::Fixture, home: &Path) -> assert_cmd::Command {
    let mut c = common::cmd(f);
    c.env("HOME", home)
        .env("DREAMD_SKIP_LAUNCHCTL", "1")
        .arg("schedule");
    c
}

fn exe() -> String {
    assert_cmd::cargo::cargo_bin("dreamd")
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
}

fn stdout(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stdout.clone()).unwrap()
}

#[test]
fn install_dry_run_prints_plist_and_writes_nothing() {
    let f = common::fixture();
    let home = TempDir::new().unwrap();
    let a = sched(&f, home.path())
        .args(["install", "--dry-run"])
        .assert()
        .code(0);
    let out = stdout(&a);
    let plist = home
        .path()
        .join("Library/LaunchAgents/dev.dreamd.nightly.plist");
    assert!(out.contains(&plist.display().to_string()), "{out}");
    for needle in [
        "<string>dream</string>",
        "<string>--all</string>",
        "<key>Hour</key><integer>3</integer>",
        "<key>Minute</key><integer>30</integer>",
        "<key>StandardOutPath</key>",
    ] {
        assert!(out.contains(needle), "missing {needle}: {out}");
    }
    let exe = exe();
    assert!(
        out.contains(&format!("<string>{exe}</string>")),
        "{exe}: {out}"
    );
    let logs = f.data_dir.join("logs").display().to_string();
    assert!(out.contains(&format!("<string>{logs}/")), "{logs}: {out}");
    assert!(!plist.exists());
}

#[test]
fn install_status_uninstall_roundtrip() {
    let f = common::fixture();
    std::fs::write(f.config_file(), "[schedule]\nhour = 4\nminute = 15\n").unwrap();
    let home = TempDir::new().unwrap();
    let plist = home
        .path()
        .join("Library/LaunchAgents/dev.dreamd.nightly.plist");

    sched(&f, home.path())
        .arg("status")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("not installed"));

    sched(&f, home.path()).arg("install").assert().code(0);
    let xml = std::fs::read_to_string(&plist).unwrap();
    assert!(xml.contains("<key>Hour</key><integer>4</integer>"), "{xml}");
    assert!(
        xml.contains("<key>Minute</key><integer>15</integer>"),
        "{xml}"
    );
    assert!(f.data_dir.join("logs").is_dir());

    let a = sched(&f, home.path()).arg("status").assert().code(0);
    let out = stdout(&a);
    assert!(out.starts_with("installed"), "{out}");
    assert!(out.contains("04:15"), "{out}");

    sched(&f, home.path()).arg("uninstall").assert().code(0);
    assert!(!plist.exists());
    let a = sched(&f, home.path()).arg("status").assert().code(0);
    assert!(stdout(&a).starts_with("not installed"));
}

fn dream_all(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = common::cmd(f);
    c.env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(120))
        .args(["dream", "--all"]);
    c
}

/// Leave only s1.jsonl in project 2 so it fails the 3-session gate.
fn starve_second(f: &common::Fixture) {
    let p2 = f
        .config_dir
        .join("projects")
        .join(format!("{}-2", common::SLUG));
    for n in ["s2.jsonl", "s3.jsonl"] {
        std::fs::remove_file(p2.join(n)).unwrap();
    }
}

#[test]
fn dream_all_one_completed_one_skipped_exits_0() {
    let f = common::fixture_with_projects(2);
    starve_second(&f);
    let a = dream_all(&f).assert().code(0);
    let out = stdout(&a);
    let skipped: Vec<&str> = out.lines().filter(|l| l.starts_with("skipped")).collect();
    let completed: Vec<&str> = out.lines().filter(|l| l.contains(" completed:")).collect();
    assert_eq!(skipped.len(), 1, "{out}");
    assert!(skipped[0].contains("-tmp-proj-2"), "{out}");
    assert_eq!(completed.len(), 1, "{out}");
    assert!(
        !out.contains("applied"),
        "auto_apply is off by default: {out}"
    );
}

#[test]
fn dream_all_every_project_skipped_exits_2() {
    let f = common::fixture_with_projects(2);
    std::fs::write(f.config_file(), "min_sessions = 10\n").unwrap();
    let a = dream_all(&f).assert().code(2);
    let out = stdout(&a);
    assert_eq!(
        out.lines().filter(|l| l.starts_with("skipped")).count(),
        2,
        "{out}"
    );
}

#[test]
fn dream_all_auto_apply_applies_completed_dreams() {
    let f = common::fixture_with_projects(2);
    std::fs::write(f.config_file(), "[schedule]\nauto_apply = true\n").unwrap();
    let a = dream_all(&f).assert().code(0);
    let out = stdout(&a);
    let completed = out.lines().filter(|l| l.contains(" completed:")).count();
    let applied: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("applied drm_"))
        .collect();
    assert_eq!(completed, 2, "{out}");
    assert_eq!(applied.len(), 2, "{out}");
    // The live memory dir now holds the dream's out/ (stub adds a file).
    assert!(f.memory_dir.join("MEMORY.md").is_file());
}
