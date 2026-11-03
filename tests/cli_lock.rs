//! Per-project dream lock and the orphaned-dream sweep, end to end against
//! the stub (`sleep` mode holds a dream in `running`).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use dreamd::config::Config;
use dreamd::db::Db;
use dreamd::model::{Dream, DreamStatus, Project, Usage};
use predicates::prelude::*;

fn project(f: &common::Fixture, slug: &str) -> Project {
    Project {
        config_dir: f.config_dir.clone(),
        slug: slug.into(),
        cwd: None,
        memory_dir: f.config_dir.join("projects").join(slug).join("memory"),
    }
}

fn lock_file(f: &common::Fixture, slug: &str) -> PathBuf {
    let cfg = Config {
        data_dir: f.data_dir.clone(),
        ..Config::default()
    };
    dreamd::lock::lock_path(&cfg, &project(f, slug))
}

fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// `dreamd dream <slug> --force` in sleep mode, in the background.
fn spawn_sleeping_dream(f: &common::Fixture, slug: &str) -> Child {
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
        .args(["dream", slug, "--force"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for(path: &Path) {
    let t = Instant::now();
    while !path.exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "{}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Insert a `running` dream row with no live owner (a crashed process).
fn insert_running(f: &common::Fixture, slug: &str) -> String {
    let db = Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    let p = project(f, slug);
    let id = Dream::new_id();
    let d = Dream {
        id: id.clone(),
        project: p.clone(),
        status: DreamStatus::Running,
        model: "sonnet".into(),
        instructions: None,
        session_ids: vec!["s1".into()],
        created_at: chrono::Utc::now(),
        ended_at: None,
        usage: Usage::default(),
        summary: None,
        error: None,
        output_dir: f.data_dir.join("dreams").join(&id).join("out"),
        diff_stats: None,
        pid: Some(dead_pid()),
    };
    db.upsert_project(&p).unwrap();
    db.insert_dream(&d).unwrap();
    id
}

#[test]
fn second_dream_of_same_project_is_busy() {
    let f = common::fixture();
    let lock = lock_file(&f, common::SLUG);
    let mut bg = spawn_sleeping_dream(&f, common::SLUG);
    wait_for(&lock);
    let pid = bg.id();
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), pid.to_string());
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "auto")
        .args(["dream", common::SLUG, "--force"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(format!(
            "busy {}: project is already being dreamed by pid {pid}",
            common::SLUG
        )));
    // --all counts busy like skipped.
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "auto")
        .args(["dream", "--all", "--force"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("busy "));
    bg.kill().unwrap();
    bg.wait().unwrap();
}

#[test]
fn other_project_proceeds_while_one_is_busy() {
    let f = common::fixture_with_projects(2);
    let one = format!("{}-1", common::SLUG);
    let two = format!("{}-2", common::SLUG);
    let mut bg = spawn_sleeping_dream(&f, &one);
    wait_for(&lock_file(&f, &one));
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .args(["dream", &two, "--force"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("completed"));
    assert!(!lock_file(&f, &two).exists());
    bg.kill().unwrap();
    bg.wait().unwrap();
}

#[test]
fn stale_lock_is_reclaimed() {
    let f = common::fixture();
    let lock = lock_file(&f, common::SLUG);
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, dead_pid().to_string()).unwrap();
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .args(["dream", common::SLUG, "--force"])
        .assert()
        .code(0);
    assert!(!lock.exists());
}

#[test]
fn killed_dream_is_swept_as_orphan() {
    let f = common::fixture();
    let lock = lock_file(&f, common::SLUG);
    let mut bg = spawn_sleeping_dream(&f, common::SLUG);
    wait_for(&lock);
    // Wait for the row to reach running.
    let db = Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    let t = Instant::now();
    let id = loop {
        if let Some(d) = db.list_dreams(Some(DreamStatus::Running)).unwrap().first() {
            break d.id.clone();
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    };
    // While the owner lives, nothing is swept.
    common::cmd(&f)
        .arg("list")
        .assert()
        .code(0)
        .stderr(predicate::str::contains("swept").not());
    bg.kill().unwrap(); // SIGKILL: no cleanup, lock file stays behind
    bg.wait().unwrap();
    assert!(lock.exists());
    common::cmd(&f)
        .arg("list")
        .assert()
        .code(0)
        .stderr(predicate::str::contains(format!(
            "swept orphaned dream {id}"
        )))
        .stdout(predicate::str::contains("failed"));
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("orphaned: owning process died"));
    // The stale lock does not block the next dream.
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .args(["dream", common::SLUG, "--force"])
        .assert()
        .code(0);
}

#[test]
fn sweep_runs_before_scan_cancel_and_doctor() {
    let f = common::fixture();
    let a = insert_running(&f, common::SLUG);
    common::cmd(&f)
        .arg("scan")
        .assert()
        .code(0)
        .stderr(predicate::str::contains(format!(
            "swept orphaned dream {a}"
        )));
    let b = insert_running(&f, common::SLUG);
    common::cmd(&f)
        .args(["cancel", &b])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(format!(
            "swept orphaned dream {b}"
        )))
        .stderr(predicate::str::contains("refused: dream is failed"));
    let c = insert_running(&f, common::SLUG);
    common::cmd(&f)
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains("FAIL orphaned dreams    1 swept"))
        .stdout(predicate::str::contains(&c));
    common::cmd(&f)
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains("ok   orphaned dreams    none"));
}

#[test]
fn row_with_live_lock_holder_is_not_swept() {
    let f = common::fixture();
    let id = insert_running(&f, common::SLUG);
    let lock = lock_file(&f, common::SLUG);
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    // This test process is alive: it "owns" the dream.
    std::fs::write(&lock, std::process::id().to_string()).unwrap();
    common::cmd(&f)
        .arg("list")
        .assert()
        .code(0)
        .stderr(predicate::str::contains("swept").not())
        .stdout(predicate::str::contains("running"));
    let db = Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    assert_eq!(
        db.get_dream(&id).unwrap().unwrap().status,
        DreamStatus::Running
    );
}

#[test]
fn serve_sweeps_orphans_at_startup() {
    use std::io::{BufRead, BufReader};
    let f = common::fixture();
    let id = insert_running(&f, common::SLUG);
    let mut srv = std::process::Command::new(assert_cmd::cargo::cargo_bin("dreamd"))
        .env("DREAMD_CONFIG", f.config_file())
        .env("DREAMD_CONFIG_DIRS", &f.config_dir)
        .env("DREAMD_DATA_DIR", &f.data_dir)
        .env(
            "DREAMD_CLAUDE_BIN",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub-claude.sh"),
        )
        .env("DREAMD_NO_NOTIFY", "1")
        .args(["serve", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(srv.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.starts_with("listening on http://127.0.0.1:"), "{line}");
    srv.kill().unwrap();
    srv.wait().unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut srv.stderr.take().unwrap(), &mut err).unwrap();
    assert!(err.contains(&format!("swept orphaned dream {id}")), "{err}");
    let db = Db::open(&f.data_dir.join("dreamd.db")).unwrap();
    let d = db.get_dream(&id).unwrap().unwrap();
    assert_eq!(d.status, DreamStatus::Failed);
    assert_eq!(d.error.as_deref(), Some("orphaned: owning process died"));
}
