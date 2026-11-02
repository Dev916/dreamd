//! Discovery: walk config dirs for dreamable projects, pick candidate
//! sessions, and decide (gate) whether a project is due for a dream.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::config::{expand_tilde, Config};
use crate::db::Db;
use crate::model::{Project, SessionFile};

/// A session modified more recently than this is considered live.
pub const LIVE_WINDOW_MINUTES: i64 = 10;
/// Maximum sessions per dream (matches the hosted Dreams API limit).
pub const MAX_SESSIONS: usize = 100;

/// A dreamable project plus what a dream would consume right now.
#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub project: Project,
    pub candidate_sessions: Vec<SessionFile>,
    pub last_dreamed_at: Option<DateTime<Utc>>,
    pub hours_since: Option<f64>,
}

/// Gate verdict for one project.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    Run,
    Skip(String),
}

impl std::fmt::Display for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Gate::Run => f.write_str("run"),
            Gate::Skip(why) => write!(f, "skip ({why})"),
        }
    }
}

/// The `scan --json` / `GET /v1/projects` row.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRow {
    pub slug: String,
    pub config_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub cwd: Option<PathBuf>,
    pub candidate_sessions: usize,
    pub last_dreamed_at: Option<DateTime<Utc>>,
    pub verdict: String,
}

impl ScanRow {
    pub fn new(d: &DiscoveredProject, cfg: &Config) -> ScanRow {
        ScanRow {
            slug: d.project.slug.clone(),
            config_dir: d.project.config_dir.clone(),
            memory_dir: d.project.memory_dir.clone(),
            cwd: d.project.cwd.clone(),
            candidate_sessions: d.candidate_sessions.len(),
            last_dreamed_at: d.last_dreamed_at,
            verdict: gate(d, cfg, false).to_string(),
        }
    }

    /// One human-readable table line.
    pub fn render(&self) -> String {
        let last = self
            .last_dreamed_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|| "never".into());
        format!(
            "{}  sessions={}  last={}  verdict={}  dir={}",
            self.slug,
            self.candidate_sessions,
            last,
            self.verdict,
            self.config_dir.display()
        )
    }
}

/// Walk every config dir's `projects/*/` and return the dreamable projects
/// (memory dir with at least one `.md`, slug not excluded), sorted by
/// config dir then slug.
pub fn discover(
    cfg: &Config,
    db: &Db,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<DiscoveredProject>> {
    let mut out = Vec::new();
    for config_dir in &cfg.config_dirs {
        let projects_dir = config_dir.join("projects");
        let Ok(entries) = std::fs::read_dir(&projects_dir) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for project_dir in dirs {
            let Some(slug) = project_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if cfg.exclude_slugs.iter().any(|x| x == slug) {
                continue;
            }
            let memory_dir = memory_dir_for(config_dir, slug);
            if !has_md(&memory_dir) {
                continue;
            }
            let project = Project {
                config_dir: config_dir.clone(),
                slug: slug.to_string(),
                cwd: project_cwd(&project_dir),
                memory_dir,
            };
            let last = db.last_dreamed_at(&project)?;
            let hours_since = last.map(|t| (now - t).num_seconds() as f64 / 3600.0);
            out.push(DiscoveredProject {
                candidate_sessions: candidate_sessions(&project_dir, last, now),
                project,
                last_dreamed_at: last,
                hours_since,
            });
        }
    }
    Ok(out)
}

/// Find the projects a `dream` selector names: a slug, or a path compared
/// with each project's cwd. `None` selector = every project (`--all`).
pub fn select_projects(
    found: Vec<DiscoveredProject>,
    selector: Option<&str>,
) -> Vec<DiscoveredProject> {
    let Some(sel) = selector else {
        return found;
    };
    let as_path = std::path::Path::new(sel);
    let canon = std::fs::canonicalize(as_path).ok();
    let by_slug: Vec<_> = found
        .iter()
        .filter(|d| d.project.slug == sel)
        .cloned()
        .collect();
    if !by_slug.is_empty() {
        return by_slug.into_iter().take(1).collect();
    }
    found
        .into_iter()
        .filter(|d| {
            d.project
                .cwd
                .as_deref()
                .is_some_and(|c| c == as_path || canon.as_deref().is_some_and(|k| k == c))
        })
        .take(1)
        .collect()
}

fn has_md(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == "md") && e.path().is_file())
        })
        .unwrap_or(false)
}

/// `*.jsonl` directly in `project_dir` with `mtime > since` (all when
/// `since` is `None`) and at least 10 minutes old, newest first, capped at 100.
pub fn candidate_sessions(
    project_dir: &Path,
    since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<SessionFile> {
    let Ok(rd) = std::fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let live_cutoff = now - Duration::minutes(LIVE_WINDOW_MINUTES);
    let mut files: Vec<SessionFile> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|path| {
            let mtime: DateTime<Utc> = std::fs::metadata(&path).ok()?.modified().ok()?.into();
            let id = path.file_stem()?.to_str()?.to_string();
            Some(SessionFile { id, path, mtime })
        })
        .filter(|s| since.is_none_or(|t| s.mtime > t) && s.mtime <= live_cutoff)
        .collect();
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
    files.truncate(MAX_SESSIONS);
    files
}

/// Should this project be dreamed now? `force` always runs; a project that
/// was never dreamed satisfies the hours requirement.
pub fn gate(d: &DiscoveredProject, cfg: &Config, force: bool) -> Gate {
    if force {
        return Gate::Run;
    }
    if let Some(h) = d.hours_since {
        if h < cfg.min_hours as f64 {
            return Gate::Skip(format!(
                "need {}h since last dream, have {:.1}h",
                cfg.min_hours, h
            ));
        }
    }
    let n = d.candidate_sessions.len();
    if n < cfg.min_sessions {
        return Gate::Skip(format!("need {} sessions, have {}", cfg.min_sessions, n));
    }
    Gate::Run
}

/// The project's memory dir: `autoMemoryDirectory` from
/// `<config_dir>/settings.json` (with `~` expansion) if set, else
/// `<config_dir>/projects/<slug>/memory`.
pub fn memory_dir_for(config_dir: &Path, slug: &str) -> PathBuf {
    let override_dir = std::fs::read_to_string(config_dir.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("autoMemoryDirectory")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| expand_tilde(Path::new(s)))
        });
    override_dir.unwrap_or_else(|| config_dir.join("projects").join(slug).join("memory"))
}

/// The project's working directory: the first JSONL record carrying a `cwd`
/// string in the newest transcript.
pub fn project_cwd(project_dir: &Path) -> Option<PathBuf> {
    let newest = std::fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by(|a, b| a.0.cmp(&b.0))?
        .1;
    let text = std::fs::read_to_string(newest).ok()?;
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn write_session(dir: &Path, name: &str, age: Duration, now: DateTime<Utc>) -> PathBuf {
        let p = dir.join(format!("{name}.jsonl"));
        std::fs::write(&p, "{\"type\":\"user\",\"cwd\":\"/tmp/proj\"}\n").unwrap();
        let t: SystemTime = (now - age).into();
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(t)
            .unwrap();
        p
    }

    fn discovered(n: usize, hours_since: Option<f64>) -> DiscoveredProject {
        let now = Utc::now();
        DiscoveredProject {
            project: Project {
                config_dir: "/c".into(),
                slug: "-p".into(),
                cwd: None,
                memory_dir: "/c/projects/-p/memory".into(),
            },
            candidate_sessions: (0..n)
                .map(|i| SessionFile {
                    id: format!("s{i}"),
                    path: format!("/c/s{i}.jsonl").into(),
                    mtime: now,
                })
                .collect(),
            last_dreamed_at: hours_since.map(|h| now - Duration::minutes((h * 60.0) as i64)),
            hours_since,
        }
    }

    #[test]
    fn candidates_exclude_recent_and_older_than_since() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        write_session(tmp.path(), "live", Duration::minutes(2), now);
        write_session(tmp.path(), "old", Duration::hours(30), now);
        write_session(tmp.path(), "fresh", Duration::hours(2), now);
        std::fs::write(tmp.path().join("notes.txt"), "x").unwrap();
        let since = Some(now - Duration::hours(24));
        let ids: Vec<_> = candidate_sessions(tmp.path(), since, now)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["fresh"]);
        let all: Vec<_> = candidate_sessions(tmp.path(), None, now)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(all, vec!["fresh", "old"]);
    }

    #[test]
    fn candidates_capped_at_100_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        for i in 0..105 {
            write_session(tmp.path(), &format!("s{i:03}"), Duration::hours(1 + i), now);
        }
        let c = candidate_sessions(tmp.path(), None, now);
        assert_eq!(c.len(), 100);
        assert_eq!(c[0].id, "s000");
        assert_eq!(c[99].id, "s099");
        assert!(c.windows(2).all(|w| w[0].mtime >= w[1].mtime));
    }

    #[test]
    fn gate_force_overrides() {
        let cfg = Config::default();
        assert_eq!(gate(&discovered(0, Some(1.0)), &cfg, true), Gate::Run);
    }

    #[test]
    fn gate_needs_min_sessions() {
        let cfg = Config::default();
        let g = gate(&discovered(2, None), &cfg, false);
        assert_eq!(g, Gate::Skip("need 3 sessions, have 2".into()));
        assert_eq!(g.to_string(), "skip (need 3 sessions, have 2)");
        assert_eq!(gate(&discovered(3, None), &cfg, false), Gate::Run);
    }

    #[test]
    fn gate_needs_min_hours_when_previously_dreamed() {
        let cfg = Config::default();
        match gate(&discovered(5, Some(3.0)), &cfg, false) {
            Gate::Skip(why) => assert!(why.starts_with("need 24h since last dream"), "{why}"),
            Gate::Run => panic!("expected skip"),
        }
        assert_eq!(gate(&discovered(5, Some(25.0)), &cfg, false), Gate::Run);
    }

    #[test]
    fn memory_dir_override_from_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let cd = tmp.path();
        assert_eq!(
            memory_dir_for(cd, "-x"),
            cd.join("projects").join("-x").join("memory")
        );
        std::fs::write(
            cd.join("settings.json"),
            r#"{"autoMemoryDirectory":"~/mem-override"}"#,
        )
        .unwrap();
        assert_eq!(
            memory_dir_for(cd, "-x"),
            dirs::home_dir().unwrap().join("mem-override")
        );
        std::fs::write(
            cd.join("settings.json"),
            r#"{"autoMemoryDirectory":"/abs/mem"}"#,
        )
        .unwrap();
        assert_eq!(memory_dir_for(cd, "-x"), PathBuf::from("/abs/mem"));
    }

    #[test]
    fn project_cwd_reads_newest_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        write_session(tmp.path(), "a", Duration::hours(1), now);
        assert_eq!(project_cwd(tmp.path()), Some(PathBuf::from("/tmp/proj")));
        assert_eq!(project_cwd(&tmp.path().join("nope")), None);
    }
}
