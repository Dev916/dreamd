//! Incremental per-session mining (session marks) and the empty-aggregator
//! skip.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{append_turns, cmd, fixture, Fixture, SLUG};

fn out(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stdout.clone()).unwrap()
}

fn err(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stderr.clone()).unwrap()
}

fn dream_cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = cmd(f);
    c.env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .args(["dream", SLUG, "--force"]);
    c
}

fn dream_id(stdout: &str) -> String {
    let i = stdout.find("drm_").expect("dream id in stdout");
    stdout[i..].split_whitespace().next().unwrap().to_string()
}

fn scan_line(f: &Fixture) -> String {
    let a = cmd(f).arg("scan").assert().code(0);
    out(&a)
        .lines()
        .find(|l| l.starts_with(SLUG))
        .unwrap()
        .to_string()
}

fn db(f: &Fixture) -> dreamd::db::Db {
    dreamd::db::Db::open(&f.data_dir.join("dreamd.db")).unwrap()
}

fn project(f: &Fixture) -> dreamd::model::Project {
    let db = db(f);
    let id = db.list_dreams(None, true).unwrap()[0].id.clone();
    db.get_dream(&id).unwrap().unwrap().project
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().len()
}

/// A first dream mines all three sessions and marks them through EOF.
fn first_dream(f: &Fixture) -> String {
    let a = dream_cmd(f).assert().code(0);
    let s = out(&a);
    assert!(s.contains("completed: 3 sessions mined ("), "{s}");
    let id = dream_id(&s);
    cmd(f).args(["archive", &id]).assert().code(0);
    let line = scan_line(f);
    assert!(line.contains("sessions=0  selected=0  new=0B"), "{line}");
    let marks = db(f).session_marks(&project(f)).unwrap();
    for t in f.transcripts() {
        let sid = t.file_stem().unwrap().to_str().unwrap();
        assert_eq!(marks[sid].mined_through_bytes, len(&t), "{sid}");
    }
    id
}

#[test]
fn grown_session_is_mined_as_a_delta() {
    let f = fixture();
    first_dream(&f);
    let s2 = f.project_dir.join("s2.jsonl");
    let added = append_turns(&s2, "grown", 12 * 1024);
    let line = scan_line(&f);
    assert!(line.contains("sessions=1  selected=1  new=12KB"), "{line}");

    // Empty findings skip the aggregator, so the captured prompt is s2's.
    let prompt = f.root.path().join("prompt.txt");
    let a = dream_cmd(&f)
        .env("DREAMD_STUB_PROMPT_OUT", &prompt)
        .env("DREAMD_STUB_EMPTY_FINDINGS", "1")
        .assert()
        .code(0);
    let s = out(&a);
    assert!(s.contains("completed: 1 sessions mined (12KB read)"), "{s}");
    let p = std::fs::read_to_string(&prompt).unwrap();
    assert!(p.contains("session id: s2"), "{p}");
    assert!(p.contains("# earlier context (already consolidated"), "{p}");
    assert!(p.contains("Mine only the new part"), "{p}");
    assert!(
        p.contains("grown turn 0") && p.contains("grown turn 7"),
        "{p}"
    );
    // The original turns appear only as earlier context (the fixture has
    // just 5 text records, all inside the 6-record overlap), never in the
    // new part.
    let new_part = &p[p.find("\n# new since the last dream\n").unwrap()..];
    assert!(
        !new_part.contains("Run the test suite and fix failures"),
        "{p}"
    );
    assert!(!new_part.contains("Prefer small commits"), "{p}");
    assert!(p.contains("Prefer small commits"), "overlap: {p}");
    let id = dream_id(&s);
    let a = cmd(&f).args(["show", &id]).assert().code(0);
    assert!(out(&a).contains("bytes read 12KB"), "{}", out(&a));
    let m = db(&f).session_mark(&project(&f), "s2").unwrap().unwrap();
    assert_eq!(m.mined_through_bytes, len(&s2));
    assert!(added >= 12 * 1024);
    let line = scan_line(&f);
    assert!(line.contains("sessions=0  selected=0  new=0B"), "{line}");

    // Second growth, with findings: the aggregator runs on the one delta,
    // and with enough history the original first user turn is gone from
    // the digest entirely (only the last 6 text records are context).
    cmd(&f).args(["archive", &id]).assert().code(0);
    append_turns(&s2, "again", 12 * 1024);
    let prompts = f.root.path().join("prompts");
    let a = dream_cmd(&f)
        .env("DREAMD_STUB_ARGV_OUT", &prompts)
        .assert()
        .code(0);
    let s = out(&a);
    assert!(
        s.contains("completed: 1 sessions mined (12KB read), +1 -0 ~1 files"),
        "{s}"
    );
    let argv = std::fs::read_to_string(&prompts).unwrap();
    // Only the s2 worker's prompt carries transcript text.
    let worker = &argv;
    assert!(worker.contains("session id: s2"), "{worker}");
    assert!(worker.contains("# earlier context"), "{worker}");
    assert!(worker.contains("again turn 0"), "{worker}");
    assert!(
        !worker.contains("Run the test suite and fix failures"),
        "{worker}"
    );
    assert!(!worker.contains("grown turn 0:"), "{worker}");
}

#[test]
fn small_growth_is_not_a_candidate() {
    let f = fixture();
    first_dream(&f);
    append_turns(&f.project_dir.join("s2.jsonl"), "tiny", 2 * 1024);
    let line = scan_line(&f);
    assert!(line.contains("sessions=0  selected=0  new=0B"), "{line}");
    let a = dream_cmd(&f).assert().code(2);
    assert!(out(&a).contains("no candidate sessions"), "{}", out(&a));
}

#[test]
fn truncated_session_is_read_whole_again() {
    let f = fixture();
    first_dream(&f);
    let s2 = f.project_dir.join("s2.jsonl");
    append_turns(&s2, "grown", 12 * 1024);
    dream_cmd(&f).assert().code(0);
    let id = db(&f).list_dreams(None, false).unwrap()[0].id.clone();
    cmd(&f).args(["archive", &id]).assert().code(0);
    // Rewrite s2 shorter than its mark (the original padded fixture).
    common::copy_padded("s2.jsonl", &s2);
    common::set_age(&s2, Duration::from_secs(3600));
    let line = scan_line(&f);
    assert!(line.contains("sessions=1  selected=1"), "{line}");
    let prompt = f.root.path().join("prompt.txt");
    let a = dream_cmd(&f)
        .env("DREAMD_STUB_PROMPT_OUT", &prompt)
        .env("DREAMD_STUB_EMPTY_FINDINGS", "1")
        .assert()
        .code(0);
    assert!(
        err(&a).contains("re-reading it from the start"),
        "{}",
        err(&a)
    );
    let p = std::fs::read_to_string(&prompt).unwrap();
    assert!(p.contains("Run the test suite and fix failures"), "{p}");
    assert!(!p.contains("# earlier context"), "{p}");
    let m = db(&f).session_mark(&project(&f), "s2").unwrap().unwrap();
    assert_eq!(m.mined_through_bytes, len(&s2));
}

#[test]
fn failed_worker_keeps_its_old_mark() {
    let f = fixture();
    first_dream(&f);
    let old = db(&f).session_mark(&project(&f), "s2").unwrap().unwrap();
    for s in ["s1", "s2", "s3"] {
        append_turns(
            &f.project_dir.join(format!("{s}.jsonl")),
            "grown",
            12 * 1024,
        );
    }
    let a = dream_cmd(&f)
        .env("DREAMD_STUB_FAIL_SESSION", "s2")
        .assert()
        .code(0);
    assert!(
        out(&a).contains("completed: 2 sessions mined"),
        "{}",
        out(&a)
    );
    let id = dream_id(&out(&a));
    cmd(&f).args(["archive", &id]).assert().code(0);
    let marks = db(&f).session_marks(&project(&f)).unwrap();
    assert_eq!(marks["s2"], old);
    for s in ["s1", "s3"] {
        assert_eq!(
            marks[s].mined_through_bytes,
            len(&f.project_dir.join(format!("{s}.jsonl")))
        );
    }
    // s2 is a candidate again (from its old mark); s1 and s3 are not.
    let a = cmd(&f).args(["scan", "--json"]).assert().code(0);
    let v: serde_json::Value = serde_json::from_str(&out(&a)).unwrap();
    assert_eq!(v[0]["candidate_sessions"], 1, "{v}");
    let a = dream_cmd(&f).assert().code(0);
    let id = dream_id(&out(&a));
    let a = cmd(&f).args(["show", &id]).assert().code(0);
    let show = out(&a);
    assert!(show.contains("  s2  ok"), "{show}");
    assert!(
        !show.contains("  s1  ") && !show.contains("  s3  "),
        "{show}"
    );
}

#[test]
fn empty_findings_skip_the_aggregator() {
    let f = fixture();
    let argv = f.root.path().join("argv.txt");
    let a = dream_cmd(&f)
        .env("DREAMD_STUB_EMPTY_FINDINGS", "1")
        .env("DREAMD_STUB_ARGV_OUT", &argv)
        .assert()
        .code(0);
    let s = out(&a);
    assert!(
        s.contains("completed: 3 sessions mined (")
            && s.contains("read), nothing to consolidate (aggregator skipped, archived)"),
        "{s}"
    );
    let id = dream_id(&s);
    let a = cmd(&f).args(["show", &id]).assert().code(0);
    assert!(out(&a).contains("completed"), "{}", out(&a));
    let a = cmd(&f).args(["diff", &id]).assert().code(0);
    assert_eq!(out(&a), "");
    let a = cmd(&f).args(["summary", &id]).assert().code(0);
    assert_eq!(
        out(&a),
        "Nothing to consolidate: 3 session(s) mined, no new insights.\n"
    );
    let argv = std::fs::read_to_string(&argv).unwrap();
    assert!(!argv.contains("acceptEdits"), "{argv}");
    assert_eq!(argv.matches("--json-schema").count(), 3, "{argv}");
    let d = db(&f).get_dream(&id).unwrap().unwrap();
    let st = d.diff_stats.unwrap();
    assert_eq!(
        (st.files_added, st.files_removed, st.files_changed),
        (0, 0, 0)
    );
    let p = project(&f);
    assert!(db(&f).last_mined_at(&p).unwrap().is_some());
    assert_eq!(db(&f).session_marks(&p).unwrap().len(), 3);
    let line = scan_line(&f);
    assert!(!line.contains("last=never"), "{line}");
    // Empty dreams are auto-archived, so the project is not parked for review.
    assert!(d.archived_at.is_some(), "empty dream should be archived");
    assert!(!line.contains("awaiting review"), "{line}");
}

#[test]
fn pre_marks_database_treats_sessions_as_unmarked() {
    let f = fixture();
    // A v0.1.0-shaped database: no session_marks table, no bytes_read.
    std::fs::create_dir_all(&f.data_dir).unwrap();
    let c = rusqlite::Connection::open(f.data_dir.join("dreamd.db")).unwrap();
    c.execute_batch(
        "CREATE TABLE projects (key TEXT PRIMARY KEY, config_dir TEXT NOT NULL,
             slug TEXT NOT NULL, cwd TEXT, memory_dir TEXT NOT NULL, last_dreamed_at TEXT);
         CREATE TABLE dream_sessions (dream_id TEXT NOT NULL, session_id TEXT NOT NULL,
             transcript_path TEXT NOT NULL, status TEXT NOT NULL, findings_path TEXT,
             usage_json TEXT NOT NULL, error TEXT, duration_ms INTEGER NOT NULL,
             PRIMARY KEY (dream_id, session_id));",
    )
    .unwrap();
    drop(c);
    let line = scan_line(&f);
    assert!(line.contains("sessions=3  selected=3  new=31KB"), "{line}");
    let prompt = f.root.path().join("prompt.txt");
    dream_cmd(&f)
        .env("DREAMD_STUB_PROMPT_OUT", &prompt)
        .env("DREAMD_STUB_EMPTY_FINDINGS", "1")
        .assert()
        .code(0);
    assert!(!std::fs::read_to_string(&prompt)
        .unwrap()
        .contains("# earlier context"));
}
