//! `dreamd mine` (hidden debug command): Task 6 acceptance criteria,
//! run against `tests/stub-claude.sh` in `worker` / `fail` / `sleep` modes.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use predicates::prelude::*;

/// Parse `<dream_dir>/findings/<sid>.json` as Findings-shaped JSON and
/// return the first insight's evidence session id.
fn evidence_session(dream_dir: &Path, sid: &str) -> String {
    let p = dream_dir.join("findings").join(format!("{sid}.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for k in ["insights", "stale", "corrections"] {
        assert!(v[k].is_array(), "{k} missing in {}", p.display());
    }
    v["insights"][0]["evidence"]["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn mine(f: &common::Fixture, dream_dir: &Path) -> assert_cmd::Command {
    let mut c = common::cmd(f);
    c.arg("mine").arg("--dream-dir").arg(dream_dir);
    for t in f.transcripts() {
        c.arg(t);
    }
    c.timeout(Duration::from_secs(60));
    c
}

#[test]
fn worker_mode_writes_findings_for_every_session() {
    let f = common::fixture();
    let dream = f.root.path().join("dream");
    mine(&f, &dream)
        .env("DREAMD_STUB_MODE", "worker")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("3 ok, 0 failed"));
    for sid in ["s1", "s2", "s3"] {
        assert_eq!(evidence_session(&dream, sid), sid);
    }
}

#[test]
fn all_fail_exits_4() {
    let f = common::fixture();
    let dream = f.root.path().join("dream");
    mine(&f, &dream)
        .env("DREAMD_STUB_MODE", "fail")
        .assert()
        .code(4)
        .stdout(predicate::str::contains("0 ok, 3 failed"));
}

#[test]
fn one_failing_session_does_not_fail_the_run() {
    let f = common::fixture();
    let dream = f.root.path().join("dream");
    mine(&f, &dream)
        .env("DREAMD_STUB_MODE", "worker")
        .env("DREAMD_STUB_FAIL_SESSION", "s2")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("2 ok, 1 failed"))
        .stdout(predicate::str::contains("s2  failed"));
    assert!(!dream.join("findings/s2.json").exists());
    assert_eq!(evidence_session(&dream, "s1"), "s1");
    assert_eq!(evidence_session(&dream, "s3"), "s3");
}

#[test]
fn sleep_mode_times_out_each_session_quickly() {
    let f = common::fixture();
    std::fs::write(f.config_file(), "[worker]\nconcurrency = 1\n").unwrap();
    let dream = f.root.path().join("dream");
    let start = Instant::now();
    let out = mine(&f, &dream)
        .env("DREAMD_STUB_MODE", "sleep")
        .arg("--timeout")
        .arg("1")
        .assert()
        .code(4)
        .stdout(predicate::str::contains("0 ok, 3 failed"))
        .get_output()
        .stdout
        .clone();
    let elapsed = start.elapsed();
    let out = String::from_utf8(out).unwrap();
    for sid in ["s1", "s2", "s3"] {
        assert!(
            out.contains(&format!("{sid}  timed_out")),
            "{sid} not timed_out:\n{out}"
        );
    }
    assert!(elapsed < Duration::from_secs(6), "took {elapsed:?}");
}
