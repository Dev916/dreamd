//! Quota caps: `[limits]`, `--since-days`, `--max-sessions`, the `--all`
//! run plan and `--dry-run` planning.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{add_transcripts, age_project, cmd, fixture, fixture_with_projects, set_age, SLUG};

fn out(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(a.get_output().stdout.clone()).unwrap()
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

/// Session ids listed by `show <id>`.
fn shown_sessions(f: &common::Fixture, id: &str) -> Vec<String> {
    let a = cmd(f).args(["show", id]).assert().code(0);
    let text = out(&a);
    let mut v: Vec<String> = text
        .split("\nsessions:\n")
        .nth(1)
        .unwrap_or("")
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect();
    v.sort();
    v
}

fn findings_count(f: &common::Fixture, id: &str) -> usize {
    std::fs::read_dir(f.data_dir.join("dreams").join(id).join("findings"))
        .unwrap()
        .count()
}

fn dreams_dir_entries(f: &common::Fixture) -> usize {
    std::fs::read_dir(f.data_dir.join("dreams"))
        .map(|rd| rd.count())
        .unwrap_or(0)
}

#[test]
fn config_show_prints_limits_and_overrides() {
    let f = fixture();
    let a = cmd(&f).args(["config", "show"]).assert().code(0);
    let s = out(&a);
    for line in [
        "[limits]",
        "max_sessions_per_dream = 10",
        "max_sessions_per_run = 30",
        "max_projects_per_run = 5",
        "backfill_days = 14",
        "min_transcript_bytes = 8192",
    ] {
        assert!(s.contains(line), "missing {line}: {s}");
    }
    std::fs::write(f.config_file(), "[limits]\nmax_sessions_per_dream = 2\n").unwrap();
    let a = cmd(&f).args(["config", "show"]).assert().code(0);
    let s = out(&a);
    assert!(s.contains("max_sessions_per_dream = 2"), "{s}");
    assert!(s.contains("max_sessions_per_run = 30"), "{s}");
}

#[test]
fn per_dream_cap_selects_newest_and_max_sessions_overrides() {
    let f = fixture();
    add_transcripts(&f.project_dir, 6);
    std::fs::write(f.config_file(), "[limits]\nmax_sessions_per_dream = 2\n").unwrap();
    let a = cmd(&f).arg("scan").assert().code(0);
    assert!(out(&a).contains("sessions=6  selected=2"), "{}", out(&a));

    let a = dream_cmd(&f).args([SLUG, "--force"]).assert().code(0);
    let s = out(&a);
    assert!(s.contains("completed: 2 sessions mined"), "{s}");
    let id = dream_id(&s);
    assert_eq!(shown_sessions(&f, &id), vec!["s5", "s6"]);
    assert_eq!(findings_count(&f, &id), 2);

    let a = dream_cmd(&f)
        .args([SLUG, "--force", "--max-sessions", "4"])
        .assert()
        .code(0);
    let s = out(&a);
    assert!(s.contains("completed: 4 sessions mined"), "{s}");
    let id = dream_id(&s);
    // s1..s3 share an mtime; ties break by id.
    assert_eq!(shown_sessions(&f, &id), vec!["s1", "s4", "s5", "s6"]);
    assert_eq!(findings_count(&f, &id), 4);
}

#[test]
fn backfill_window_since_days_and_min_bytes() {
    let f = fixture();
    add_transcripts(&f.project_dir, 6);
    for i in 4..=6 {
        set_age(
            &f.project_dir.join(format!("s{i}.jsonl")),
            Duration::from_secs(20 * 86400 + i * 60),
        );
    }
    let a = cmd(&f).arg("scan").assert().code(0);
    assert!(out(&a).contains("sessions=3  selected=3"), "{}", out(&a));

    // A tiny transcript is never a candidate.
    let tiny = f.project_dir.join("tiny.jsonl");
    std::fs::write(&tiny, format!("{:<99}\n", "{\"type\":\"user\"}")).unwrap();
    assert_eq!(std::fs::metadata(&tiny).unwrap().len(), 100);
    set_age(&tiny, Duration::from_secs(3600));
    let a = cmd(&f).arg("scan").assert().code(0);
    assert!(out(&a).contains("sessions=3  selected=3"), "{}", out(&a));
    std::fs::write(f.config_file(), "[limits]\nmin_transcript_bytes = 0\n").unwrap();
    let a = cmd(&f).arg("scan").assert().code(0);
    assert!(out(&a).contains("sessions=4  selected=4"), "{}", out(&a));
    std::fs::remove_file(f.config_file()).unwrap();

    let a = dream_cmd(&f)
        .args([SLUG, "--since-days", "0"])
        .assert()
        .code(0);
    let s = out(&a);
    assert!(s.contains("completed: 6 sessions mined"), "{s}");
    let id = dream_id(&s);
    assert_eq!(
        shown_sessions(&f, &id),
        vec!["s1", "s2", "s3", "s4", "s5", "s6"]
    );
}

/// Three projects, `-tmp-proj-3` most recently active, `-tmp-proj-1` least.
fn three_projects() -> common::Fixture {
    let f = fixture_with_projects(3);
    for i in 1..=3u64 {
        let dir: PathBuf = f.config_dir.join("projects").join(format!("{SLUG}-{i}"));
        age_project(&dir, Duration::from_secs(4 * 3600 - i * 600));
    }
    std::fs::write(
        f.config_file(),
        "[limits]\nmax_sessions_per_run = 4\nmax_projects_per_run = 2\nmax_sessions_per_dream = 3\n",
    )
    .unwrap();
    f
}

#[test]
fn all_dry_run_prints_plan_and_runs_nothing() {
    let f = three_projects();
    let a = dream_cmd(&f).args(["--all", "--dry-run"]).assert().code(0);
    let s = out(&a);
    assert!(s.contains("plan -tmp-proj-3: 3 of 3 sessions"), "{s}");
    assert!(s.contains("plan -tmp-proj-2: 1 of 3 sessions"), "{s}");
    assert!(
        s.contains("deferred -tmp-proj-1: run budget exhausted (3 candidates)"),
        "{s}"
    );
    assert!(
        s.contains("plan: 2 projects, 4 worker calls, 2 aggregator calls"),
        "{s}"
    );
    assert_eq!(dreams_dir_entries(&f), 0);
    let a = cmd(&f).arg("list").assert().code(0);
    assert!(out(&a).contains("no dreams"), "{}", out(&a));
}

#[test]
fn all_runs_the_plan_and_defers_the_rest() {
    let f = three_projects();
    let a = dream_cmd(&f).arg("--all").assert().code(0);
    let s = out(&a);
    assert!(
        s.contains("deferred -tmp-proj-1: run budget exhausted (3 candidates)"),
        "{s}"
    );
    let mined: Vec<&str> = s.lines().filter(|l| l.contains("completed:")).collect();
    assert_eq!(mined.len(), 2, "{s}");
    assert!(mined[0].contains("completed: 3 sessions mined"), "{s}");
    assert!(mined[1].contains("completed: 1 sessions mined"), "{s}");
    assert_eq!(dreams_dir_entries(&f), 2);
    let a = cmd(&f).arg("list").assert().code(0);
    let l = out(&a);
    assert!(
        l.contains("-tmp-proj-3") && l.contains("-tmp-proj-2"),
        "{l}"
    );
    assert!(!l.contains("-tmp-proj-1"), "{l}");
}

#[test]
fn single_dry_run_prints_plan() {
    let f = fixture();
    let a = dream_cmd(&f).args([SLUG, "--dry-run"]).assert().code(0);
    let s = out(&a);
    assert!(s.contains("plan -tmp-proj: 3 of 3 sessions"), "{s}");
    assert!(
        s.contains("plan: 1 projects, 3 worker calls, 1 aggregator calls"),
        "{s}"
    );
    assert_eq!(dreams_dir_entries(&f), 0);
    // Gate-skipped: empty plan, exit 2.
    std::fs::write(f.config_file(), "min_sessions = 10\n").unwrap();
    let a = dream_cmd(&f).args([SLUG, "--dry-run"]).assert().code(2);
    let s = out(&a);
    assert!(
        s.contains("skipped -tmp-proj: need 10 sessions, have 3"),
        "{s}"
    );
    assert!(
        s.contains("plan: 0 projects, 0 worker calls, 0 aggregator calls"),
        "{s}"
    );
}

#[test]
fn default_caps_plan_five_projects() {
    let f = fixture_with_projects(7);
    let a = dream_cmd(&f).args(["--all", "--dry-run"]).assert().code(0);
    let s = out(&a);
    assert_eq!(
        s.lines().filter(|l| l.starts_with("plan -")).count(),
        5,
        "{s}"
    );
    assert_eq!(
        s.lines().filter(|l| l.starts_with("deferred ")).count(),
        2,
        "{s}"
    );
    assert!(
        s.contains("plan: 5 projects, 15 worker calls, 5 aggregator calls"),
        "{s}"
    );
    let a = cmd(&f).arg("scan").assert().code(0);
    let s = out(&a);
    assert!(
        s.contains("next --all run: 5 projects, 15 worker calls (caps: "),
        "{s}"
    );
}
