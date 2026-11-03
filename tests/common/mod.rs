//! Shared integration-test fixture: a temp Claude config dir with one
//! project (`-tmp-proj`) — or n (`fixture_with_projects`) — each with a
//! memory dir and three aged transcripts.
// Each test crate uses a different subset of these helpers.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use filetime::FileTime;
use tempfile::TempDir;

pub const SLUG: &str = "-tmp-proj";

pub struct Fixture {
    pub root: TempDir,
    pub config_dir: PathBuf,
    pub project_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Fixture {
    /// `<root>/config.toml` — `cmd()` points `DREAMD_CONFIG` here (absent by default).
    pub fn config_file(&self) -> PathBuf {
        self.root.path().join("config.toml")
    }

    /// Transcript paths in the project dir, sorted.
    pub fn transcripts(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&self.project_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        v.sort();
        v
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts")
}

fn set_mtime(path: &Path, t: SystemTime) {
    filetime::set_file_mtime(path, FileTime::from_system_time(t)).unwrap();
}

/// Build the fixture. Transcript mtimes are set to now - 2h.
pub fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let config_dir = root.path().join("claude");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let project_dir = add_project(&config_dir, SLUG);
    let memory_dir = project_dir.join("memory");
    Fixture {
        root,
        config_dir,
        project_dir,
        memory_dir,
        data_dir,
    }
}

/// `n` projects `-tmp-proj-1..=n`, each with memory + 3 aged transcripts.
/// `project_dir`/`memory_dir` point at `-tmp-proj-1`.
pub fn fixture_with_projects(n: usize) -> Fixture {
    assert!(n >= 1);
    let root = TempDir::new().unwrap();
    let config_dir = root.path().join("claude");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut first = None;
    for i in 1..=n {
        let dir = add_project(&config_dir, &format!("{SLUG}-{i}"));
        first.get_or_insert(dir);
    }
    let project_dir = first.unwrap();
    let memory_dir = project_dir.join("memory");
    Fixture {
        root,
        config_dir,
        project_dir,
        memory_dir,
        data_dir,
    }
}

/// `<config_dir>/projects/<slug>` with a memory dir and 3 aged transcripts.
pub fn add_project(config_dir: &Path, slug: &str) -> PathBuf {
    let project_dir = config_dir.join("projects").join(slug);
    let memory_dir = project_dir.join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
    for name in ["s1.jsonl", "s2.jsonl", "s3.jsonl"] {
        let dst = project_dir.join(name);
        copy_padded(name, &dst);
        set_mtime(&dst, two_hours_ago);
    }

    std::fs::write(
        memory_dir.join("MEMORY.md"),
        "- [Fact A](fact-a.md) — scripts live under tools/\n\
         - [Fact B](fact-b.md) — deploy config lives in infra/\n",
    )
    .unwrap();
    std::fs::write(
        memory_dir.join("fact-a.md"),
        "---\nname: fact-a\ndescription: Scripts live under tools/\nmetadata:\n  type: project\n---\n\nBuild and helper scripts live under `tools/`, not `scripts/`.\n",
    )
    .unwrap();
    std::fs::write(
        memory_dir.join("fact-b.md"),
        "---\nname: fact-b\ndescription: Deploy config lives in infra/\nmetadata:\n  type: project\n---\n\nDeploy configuration is `infra/deploy.yaml`.\n",
    )
    .unwrap();
    project_dir
}

/// Padding so fixture transcripts clear the default `min_transcript_bytes`
/// (8192): one `progress` record, which the reducer drops.
pub const PAD_BYTES: usize = 9000;

/// Copy fixture transcript `name` to `dst`, padded past 8 KiB.
pub fn copy_padded(name: &str, dst: &Path) {
    let mut text = std::fs::read_to_string(fixtures_dir().join(name)).unwrap();
    text.push_str(&format!(
        "{{\"type\": \"progress\", \"data\": {{\"output\": \"{}\"}}}}\n",
        "x".repeat(PAD_BYTES)
    ));
    std::fs::write(dst, text).unwrap();
}

/// Add copies of `s1.jsonl` named `s4..=s<total>` to `project_dir`; `s<i>`
/// is aged 2h - i minutes, so higher numbers are newer (all older than the
/// live window).
pub fn add_transcripts(project_dir: &Path, total: usize) {
    for i in 4..=total {
        let dst = project_dir.join(format!("s{i}.jsonl"));
        copy_padded("s1.jsonl", &dst);
        set_age(&dst, Duration::from_secs(2 * 3600 - 60 * i as u64));
    }
}

/// Set a file's mtime to now - `age`.
pub fn set_age(path: &Path, age: Duration) {
    set_mtime(path, SystemTime::now() - age);
}

/// Set every transcript in `project_dir` to now - `age`.
pub fn age_project(project_dir: &Path, age: Duration) {
    for e in std::fs::read_dir(project_dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            set_age(&p, age);
        }
    }
}

/// Set a file's mtime to now - 2h (an aged, dreamable transcript).
pub fn set_aged(path: &Path) {
    set_mtime(path, SystemTime::now() - Duration::from_secs(2 * 3600));
}

/// Set a file's mtime to now (makes a transcript "live").
pub fn touch_recent(path: &Path) {
    set_mtime(path, SystemTime::now());
}

/// `dreamd` with the fixture's env preset.
pub fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = assert_cmd::Command::cargo_bin("dreamd").unwrap();
    c.env("DREAMD_CONFIG", f.config_file())
        .env("DREAMD_CONFIG_DIRS", &f.config_dir)
        .env("DREAMD_DATA_DIR", &f.data_dir)
        .env(
            "DREAMD_CLAUDE_BIN",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub-claude.sh"),
        )
        .env("DREAMD_NO_NOTIFY", "1")
        .env_remove("DREAMD_LOG");
    c
}
