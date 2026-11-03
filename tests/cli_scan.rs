//! Task 3 acceptance: `dreamd scan` discovery and gate.

mod common;

use common::{cmd, fixture, touch_recent, SLUG};
use predicates::prelude::*;

fn stdout_of(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stdout.clone()).unwrap()
}

#[test]
fn scan_fresh_project_verdict_run() {
    let f = fixture();
    cmd(&f)
        .arg("scan")
        .assert()
        .code(0)
        .stdout(predicate::str::contains(
            "-tmp-proj  sessions=3  selected=3  new=31KB  last=never  verdict=run",
        ));
}

#[test]
fn scan_live_session_excluded_and_gate_skips() {
    let f = fixture();
    touch_recent(&f.transcripts()[0]);
    cmd(&f)
        .arg("scan")
        .assert()
        .code(0)
        .stdout(predicate::str::contains(
            "sessions=2  selected=2  new=21KB  last=never  verdict=skip (need 3 sessions, have 2)",
        ));
}

#[test]
fn scan_skips_projects_without_memory_and_excluded_slugs() {
    let f = fixture();
    let projects = f.config_dir.join("projects");
    // No memory/ subdir.
    std::fs::create_dir_all(projects.join("-no-memory")).unwrap();
    std::fs::copy(
        f.transcripts()[0].clone(),
        projects.join("-no-memory/s1.jsonl"),
    )
    .unwrap();
    // Dreamable but excluded.
    std::fs::create_dir_all(projects.join("-excluded/memory")).unwrap();
    std::fs::write(projects.join("-excluded/memory/MEMORY.md"), "- x\n").unwrap();
    std::fs::write(f.config_file(), "exclude_slugs = [\"-excluded\"]\n").unwrap();

    let a = cmd(&f).arg("scan").assert().code(0);
    let out = stdout_of(&a);
    assert!(out.contains(SLUG), "{out}");
    assert!(!out.contains("-no-memory"), "{out}");
    assert!(!out.contains("-excluded"), "{out}");
}

#[test]
fn scan_json_has_expected_keys() {
    let f = fixture();
    let a = cmd(&f).args(["scan", "--json"]).assert().code(0);
    let v: serde_json::Value = serde_json::from_str(&stdout_of(&a)).unwrap();
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    let obj = arr[0].as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "candidate_sessions",
            "config_dir",
            "cwd",
            "last_dreamed_at",
            "last_mined_at",
            "memory_dir",
            "new_bytes",
            "selected_sessions",
            "slug",
            "verdict"
        ]
    );
    assert_eq!(obj["slug"], SLUG);
    assert_eq!(obj["candidate_sessions"], 3);
    assert_eq!(obj["selected_sessions"], 3);
    let total: u64 = f
        .transcripts()
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert_eq!(obj["new_bytes"], total);
    assert_eq!(obj["cwd"], "/tmp/proj");
    assert_eq!(obj["last_dreamed_at"], serde_json::Value::Null);
    assert_eq!(obj["last_mined_at"], serde_json::Value::Null);
    assert_eq!(obj["verdict"], "run");
    assert_eq!(
        obj["memory_dir"].as_str().unwrap(),
        f.memory_dir.to_str().unwrap()
    );
}
