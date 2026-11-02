//! Shared integration-test fixture: a temp Claude config dir with one
//! project (`-tmp-proj`), a memory dir and three aged transcripts.
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
    let project_dir = config_dir.join("projects").join(SLUG);
    let memory_dir = project_dir.join("memory");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
    for name in ["s1.jsonl", "s2.jsonl", "s3.jsonl"] {
        let dst = project_dir.join(name);
        std::fs::copy(fixtures_dir().join(name), &dst).unwrap();
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

    Fixture {
        root,
        config_dir,
        project_dir,
        memory_dir,
        data_dir,
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
