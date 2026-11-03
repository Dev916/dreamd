//! Mined-through cutoff, the awaiting-review gate and `dreamd archive`.

mod common;

use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{cmd, fixture, set_age, SLUG};

fn out(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stdout.clone()).unwrap()
}

fn err(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stderr.clone()).unwrap()
}

fn dream_cmd(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = cmd(f);
    c.env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .arg("dream");
    c
}

fn dream_id(stdout: &str) -> String {
    let i = stdout.find("drm_").expect("dream id in stdout");
    stdout[i..].split_whitespace().next().unwrap().to_string()
}

fn scan_line(f: &common::Fixture) -> String {
    let a = cmd(f).arg("scan").assert().code(0);
    out(&a)
        .lines()
        .find(|l| l.starts_with(SLUG))
        .unwrap()
        .to_string()
}

#[test]
fn completed_dream_parks_project_until_archived() {
    let f = fixture();
    let a = dream_cmd(&f).args([SLUG, "--force"]).assert().code(0);
    let id = dream_id(&out(&a));

    // Parked: awaiting review, cutoff = completion time, no candidates.
    let line = scan_line(&f);
    let reason = format!("awaiting review: {id} (apply, rollback or archive it)");
    assert!(line.contains(&format!("verdict=skip ({reason})")), "{line}");
    assert!(line.contains("sessions=0"), "{line}");
    assert!(!line.contains("last=never"), "{line}");
    let a = cmd(&f).args(["scan", "--json"]).assert().code(0);
    let v: serde_json::Value = serde_json::from_str(&out(&a)).unwrap();
    assert!(v[0]["last_mined_at"].is_string(), "{v}");
    assert!(v[0]["last_dreamed_at"].is_null(), "{v}");

    let a = dream_cmd(&f).arg(SLUG).assert().code(2);
    assert!(
        out(&a).contains(&format!("skipped {SLUG}: {reason}")),
        "{}",
        out(&a)
    );
    // --sessions bypasses the gate; --force bypasses it and the cutoff.
    dream_cmd(&f)
        .args([SLUG, "--sessions", "s1", "--dry-run"])
        .assert()
        .code(0);
    let a = dream_cmd(&f).args([SLUG, "--force"]).assert().code(0);
    let id2 = dream_id(&out(&a));
    assert_ne!(id, id2);

    // Archive both: no longer awaiting review; the old transcripts stay
    // behind the cutoff.
    for i in [&id, &id2] {
        let a = cmd(&f).args(["archive", i]).assert().code(0);
        assert!(out(&a).starts_with(&format!("archived {i} (completed, at ")));
    }
    let line = scan_line(&f);
    assert!(!line.contains("awaiting review"), "{line}");
    assert!(line.contains("sessions=0  selected=0"), "{line}");
    assert!(
        line.contains("verdict=skip (need 24h since last dream"),
        "{line}"
    );

    // A transcript newer than the cutoff is a candidate.
    let fresh = f.project_dir.join("s9.jsonl");
    std::fs::copy(f.project_dir.join("s1.jsonl"), &fresh).unwrap();
    set_age(&fresh, Duration::from_secs(0));
    // Back-date the cutoff 2h so an aged (now-1h) transcript is past it.
    let db = dreamd::db::Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    let d = db.get_dream(&id).unwrap().unwrap();
    db.set_last_mined_at(&d.project, chrono::Utc::now() - chrono::Duration::hours(2))
        .unwrap();
    set_age(&fresh, Duration::from_secs(3600));
    let line = scan_line(&f);
    assert!(line.contains("sessions=1  selected=1"), "{line}");

    // list hides archived dreams; --include-archived marks them.
    let a = cmd(&f).arg("list").assert().code(0);
    assert_eq!(out(&a), "no dreams\n");
    let a = cmd(&f)
        .args(["list", "--include-archived"])
        .assert()
        .code(0);
    let s = out(&a);
    for i in [&id, &id2] {
        let l = s.lines().find(|l| l.starts_with(i.as_str())).unwrap();
        assert!(l.ends_with("  archived"), "{l}");
    }
    // show still works, with archived_at.
    let a = cmd(&f).args(["show", &id]).assert().code(0);
    assert!(out(&a).contains("archived_at  20"), "{}", out(&a));
    let a = cmd(&f).args(["show", &id, "--json"]).assert().code(0);
    let v: serde_json::Value = serde_json::from_str(&out(&a)).unwrap();
    assert!(v["archived_at"].is_string(), "{v}");
}

#[test]
fn archive_refuses_active_and_unknown_and_is_idempotent() {
    let f = fixture();
    let a = cmd(&f).args(["archive", "drm_nope"]).assert().code(3);
    assert!(err(&a).contains("dream not found: drm_nope"));

    let mut bg = spawn_sleeping_dream(&f);
    let db = dreamd::db::Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    let t = Instant::now();
    let id = loop {
        if let Some(d) = db
            .list_dreams(Some(dreamd::model::DreamStatus::Running), false)
            .unwrap()
            .first()
        {
            break d.id.clone();
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    };
    let a = cmd(&f).args(["archive", &id]).assert().code(3);
    assert_eq!(err(&a).trim(), "refused: dream is running");
    cmd(&f).args(["cancel", &id]).assert().code(0);
    let _ = bg.wait();
    // Canceled is terminal: archivable, twice.
    cmd(&f).args(["archive", &id]).assert().code(0);
    let first = db.get_dream(&id).unwrap().unwrap().archived_at;
    assert!(first.is_some());
    cmd(&f).args(["archive", &id]).assert().code(0);
    assert_eq!(db.get_dream(&id).unwrap().unwrap().archived_at, first);
    // A canceled dream never set the mined-through cutoff.
    assert!(db
        .last_mined_at(&db.get_dream(&id).unwrap().unwrap().project)
        .unwrap()
        .is_none());
}

fn spawn_sleeping_dream(f: &common::Fixture) -> Child {
    std::process::Command::new(assert_cmd::cargo::cargo_bin("dreamd"))
        .env("DREAMD_CONFIG", f.config_file())
        .env("DREAMD_CONFIG_DIRS", &f.config_dir)
        .env("DREAMD_DATA_DIR", &f.data_dir)
        .env(
            "DREAMD_CLAUDE_BIN",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub-claude.sh"),
        )
        .env("DREAMD_NO_NOTIFY", "1")
        .env("DREAMD_STUB_MODE", "sleep")
        .args(["dream", SLUG, "--force"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
