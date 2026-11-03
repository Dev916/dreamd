//! SQLite store for projects, dreams, per-session worker records and applies.
//!
//! Timestamps are stored as fixed-width RFC 3339 UTC strings (nanosecond
//! precision) so they round-trip exactly and sort lexically. Nested values
//! (`usage`, `diff_stats`, `session_ids`) are JSON text columns.
// Most methods are consumed by later stages (pipeline, apply, api).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Context};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::model::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    key             TEXT PRIMARY KEY,
    config_dir      TEXT NOT NULL,
    slug            TEXT NOT NULL,
    cwd             TEXT,
    memory_dir      TEXT NOT NULL,
    last_dreamed_at TEXT
);
CREATE TABLE IF NOT EXISTS dreams (
    id               TEXT PRIMARY KEY,
    project_key      TEXT NOT NULL REFERENCES projects(key),
    status           TEXT NOT NULL,
    model            TEXT NOT NULL,
    instructions     TEXT,
    session_ids_json TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    ended_at         TEXT,
    usage_json       TEXT NOT NULL,
    summary          TEXT,
    error            TEXT,
    output_dir       TEXT NOT NULL,
    diff_stats_json  TEXT,
    pid              INTEGER
);
CREATE INDEX IF NOT EXISTS dreams_created_at ON dreams(created_at);
CREATE TABLE IF NOT EXISTS dream_sessions (
    dream_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    transcript_path TEXT NOT NULL,
    status          TEXT NOT NULL,
    findings_path   TEXT,
    usage_json      TEXT NOT NULL,
    error           TEXT,
    duration_ms     INTEGER NOT NULL,
    PRIMARY KEY (dream_id, session_id)
);
CREATE TABLE IF NOT EXISTS applies (
    dream_id    TEXT PRIMARY KEY,
    applied_at  TEXT NOT NULL,
    backup_path TEXT NOT NULL
);
"#;

const DREAM_COLS: &str = "d.id, d.status, d.model, d.instructions, d.session_ids_json, \
     d.created_at, d.ended_at, d.usage_json, d.summary, d.error, d.output_dir, \
     d.diff_stats_json, d.pid, p.config_dir, p.slug, p.cwd, p.memory_dir";

/// The dreamd database. `Send + Sync`; every call takes the connection lock.
pub struct Db {
    conn: Mutex<Connection>,
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(s: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("bad timestamp {s:?}"))?
        .with_timezone(&Utc))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn row_to_dream(r: &Row<'_>) -> anyhow::Result<Dream> {
    let status: String = r.get(1)?;
    let session_ids: String = r.get(4)?;
    let created_at: String = r.get(5)?;
    let ended_at: Option<String> = r.get(6)?;
    let usage: String = r.get(7)?;
    let output_dir: String = r.get(10)?;
    let diff_stats: Option<String> = r.get(11)?;
    let pid: Option<i64> = r.get(12)?;
    let config_dir: String = r.get(13)?;
    let cwd: Option<String> = r.get(15)?;
    let memory_dir: String = r.get(16)?;
    Ok(Dream {
        id: r.get(0)?,
        project: Project {
            config_dir: config_dir.into(),
            slug: r.get(14)?,
            cwd: cwd.map(PathBuf::from),
            memory_dir: memory_dir.into(),
        },
        status: status.parse()?,
        model: r.get(2)?,
        instructions: r.get(3)?,
        session_ids: serde_json::from_str(&session_ids).context("session_ids_json")?,
        created_at: parse_ts(&created_at)?,
        ended_at: ended_at.as_deref().map(parse_ts).transpose()?,
        usage: serde_json::from_str(&usage).context("usage_json")?,
        summary: r.get(8)?,
        error: r.get(9)?,
        output_dir: output_dir.into(),
        diff_stats: diff_stats
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("diff_stats_json")?,
        pid: pid.map(u32::try_from).transpose().context("pid")?,
    })
}

fn row_to_session(r: &Row<'_>) -> anyhow::Result<SessionRecord> {
    let transcript_path: String = r.get(1)?;
    let status: String = r.get(2)?;
    let findings_path: Option<String> = r.get(3)?;
    let usage: String = r.get(4)?;
    let duration_ms: i64 = r.get(6)?;
    Ok(SessionRecord {
        session_id: r.get(0)?,
        transcript_path: transcript_path.into(),
        status: status.parse()?,
        findings_path: findings_path.map(PathBuf::from),
        usage: serde_json::from_str(&usage).context("usage_json")?,
        error: r.get(5)?,
        duration_ms: u64::try_from(duration_ms).context("duration_ms")?,
    })
}

fn expect_one(n: usize, what: &str, id: &str) -> anyhow::Result<()> {
    if n == 0 {
        Err(anyhow!("{what} not found: {id}"))
    } else {
        Ok(())
    }
}

impl Db {
    /// Open (creating parent dirs, file and tables as needed).
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// A private in-memory database (tests).
    #[cfg_attr(not(test), allow(dead_code))] // unit tests only
    pub fn open_memory() -> anyhow::Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Db> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA).context("create schema")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite inconsistent.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upsert_project_on(conn: &Connection, p: &Project) -> anyhow::Result<()> {
        conn.execute(
            "INSERT INTO projects (key, config_dir, slug, cwd, memory_dir)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET config_dir = excluded.config_dir,
                 slug = excluded.slug, cwd = excluded.cwd, memory_dir = excluded.memory_dir",
            params![
                p.key(),
                path_str(&p.config_dir),
                p.slug,
                p.cwd.as_deref().map(path_str),
                path_str(&p.memory_dir)
            ],
        )?;
        Ok(())
    }

    /// Insert or refresh a project's paths; never touches `last_dreamed_at`.
    pub fn upsert_project(&self, p: &Project) -> anyhow::Result<()> {
        Self::upsert_project_on(&self.conn(), p)
    }

    pub fn last_dreamed_at(&self, p: &Project) -> anyhow::Result<Option<DateTime<Utc>>> {
        let v: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT last_dreamed_at FROM projects WHERE key = ?1",
                [p.key()],
                |r| r.get(0),
            )
            .optional()?;
        v.flatten().as_deref().map(parse_ts).transpose()
    }

    pub fn set_last_dreamed_at(&self, p: &Project, at: DateTime<Utc>) -> anyhow::Result<()> {
        let conn = self.conn();
        Self::upsert_project_on(&conn, p)?;
        conn.execute(
            "UPDATE projects SET last_dreamed_at = ?2 WHERE key = ?1",
            params![p.key(), ts(at)],
        )?;
        Ok(())
    }

    /// Insert a dream (upserting its project first).
    pub fn insert_dream(&self, d: &Dream) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Self::upsert_project_on(&tx, &d.project)?;
        tx.execute(
            "INSERT INTO dreams (id, project_key, status, model, instructions, session_ids_json,
                 created_at, ended_at, usage_json, summary, error, output_dir, diff_stats_json, pid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                d.id,
                d.project.key(),
                d.status.as_str(),
                d.model,
                d.instructions,
                serde_json::to_string(&d.session_ids)?,
                ts(d.created_at),
                d.ended_at.map(ts),
                serde_json::to_string(&d.usage)?,
                d.summary,
                d.error,
                path_str(&d.output_dir),
                d.diff_stats
                    .map(|s| serde_json::to_string(&s))
                    .transpose()?,
                d.pid
            ],
        )
        .with_context(|| format!("insert dream {}", d.id))?;
        tx.commit()?;
        Ok(())
    }

    /// Set status (and error, if given). Terminal statuses stamp `ended_at`
    /// unless already set, so apply/rollback keep the original end time.
    pub fn update_status(
        &self,
        id: &str,
        status: DreamStatus,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        let ended = status.is_terminal().then(|| ts(Utc::now()));
        let n = self.conn().execute(
            "UPDATE dreams SET status = ?2, error = COALESCE(?3, error),
                 ended_at = COALESCE(ended_at, ?4)
             WHERE id = ?1",
            params![id, status.as_str(), error, ended],
        )?;
        expect_one(n, "dream", id)
    }

    /// Mark a dream `failed` with `error` only while it is still `pending`
    /// or `running` (atomic w.r.t. the pipeline's own terminal update).
    /// Returns whether it changed.
    pub fn fail_if_active(&self, id: &str, error: &str) -> anyhow::Result<bool> {
        let n = self.conn().execute(
            "UPDATE dreams SET status = 'failed', error = ?2,
                 ended_at = COALESCE(ended_at, ?3), pid = NULL
             WHERE id = ?1 AND status IN ('pending', 'running')",
            params![id, error, ts(Utc::now())],
        )?;
        Ok(n == 1)
    }

    pub fn set_result(
        &self,
        id: &str,
        usage: &Usage,
        summary: &str,
        stats: &DiffStats,
    ) -> anyhow::Result<()> {
        let n = self.conn().execute(
            "UPDATE dreams SET usage_json = ?2, summary = ?3, diff_stats_json = ?4 WHERE id = ?1",
            params![
                id,
                serde_json::to_string(usage)?,
                summary,
                serde_json::to_string(stats)?
            ],
        )?;
        expect_one(n, "dream", id)
    }

    /// Record usage without touching summary or diff stats (failed dreams).
    pub fn set_usage(&self, id: &str, usage: &Usage) -> anyhow::Result<()> {
        let n = self.conn().execute(
            "UPDATE dreams SET usage_json = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(usage)?],
        )?;
        expect_one(n, "dream", id)
    }

    pub fn set_pid(&self, id: &str, pid: Option<u32>) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute("UPDATE dreams SET pid = ?2 WHERE id = ?1", params![id, pid])?;
        expect_one(n, "dream", id)
    }

    pub fn get_dream(&self, id: &str) -> anyhow::Result<Option<Dream>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DREAM_COLS} FROM dreams d JOIN projects p ON p.key = d.project_key
             WHERE d.id = ?1"
        ))?;
        let mut rows = stmt.query([id])?;
        rows.next()?.map(row_to_dream).transpose()
    }

    /// All dreams (optionally of one status), newest first.
    pub fn list_dreams(&self, status: Option<DreamStatus>) -> anyhow::Result<Vec<Dream>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DREAM_COLS} FROM dreams d JOIN projects p ON p.key = d.project_key
             WHERE ?1 IS NULL OR d.status = ?1
             ORDER BY d.created_at DESC, d.id DESC"
        ))?;
        let mut rows = stmt.query([status.map(|s| s.as_str())])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_dream(r)?);
        }
        Ok(out)
    }

    pub fn insert_session(&self, dream_id: &str, s: &SessionRecord) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO dream_sessions (dream_id, session_id, transcript_path, status,
                 findings_path, usage_json, error, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                dream_id,
                s.session_id,
                path_str(&s.transcript_path),
                s.status.as_str(),
                s.findings_path.as_deref().map(path_str),
                serde_json::to_string(&s.usage)?,
                s.error,
                i64::try_from(s.duration_ms)?
            ],
        )?;
        Ok(())
    }

    pub fn update_session(&self, dream_id: &str, s: &SessionRecord) -> anyhow::Result<()> {
        let n = self.conn().execute(
            "UPDATE dream_sessions SET transcript_path = ?3, status = ?4, findings_path = ?5,
                 usage_json = ?6, error = ?7, duration_ms = ?8
             WHERE dream_id = ?1 AND session_id = ?2",
            params![
                dream_id,
                s.session_id,
                path_str(&s.transcript_path),
                s.status.as_str(),
                s.findings_path.as_deref().map(path_str),
                serde_json::to_string(&s.usage)?,
                s.error,
                i64::try_from(s.duration_ms)?
            ],
        )?;
        expect_one(n, "session", &format!("{dream_id}/{}", s.session_id))
    }

    /// Sessions of a dream in insertion order.
    pub fn list_sessions(&self, dream_id: &str) -> anyhow::Result<Vec<SessionRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, transcript_path, status, findings_path, usage_json, error,
                 duration_ms
             FROM dream_sessions WHERE dream_id = ?1 ORDER BY rowid",
        )?;
        let mut rows = stmt.query([dream_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_session(r)?);
        }
        Ok(out)
    }

    /// Record (or replace) the backup taken when a dream was applied.
    pub fn record_apply(&self, dream_id: &str, backup_path: &Path) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO applies (dream_id, applied_at, backup_path) VALUES (?1, ?2, ?3)
             ON CONFLICT(dream_id) DO UPDATE SET applied_at = excluded.applied_at,
                 backup_path = excluded.backup_path",
            params![dream_id, ts(Utc::now()), path_str(backup_path)],
        )?;
        Ok(())
    }

    pub fn get_apply(&self, dream_id: &str) -> anyhow::Result<Option<PathBuf>> {
        let p: Option<String> = self
            .conn()
            .query_row(
                "SELECT backup_path FROM applies WHERE dream_id = ?1",
                [dream_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(p.map(PathBuf::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn project(slug: &str) -> Project {
        Project {
            config_dir: "/cfg".into(),
            slug: slug.into(),
            cwd: Some("/work/x".into()),
            memory_dir: format!("/cfg/projects/{slug}/memory").into(),
        }
    }

    fn dream(slug: &str, created_at: DateTime<Utc>, status: DreamStatus) -> Dream {
        Dream {
            id: Dream::new_id(),
            project: project(slug),
            status,
            model: "opus".into(),
            instructions: Some("be terse".into()),
            session_ids: vec!["s1".into(), "s2".into()],
            created_at,
            ended_at: None,
            usage: Usage::default(),
            summary: None,
            error: None,
            output_dir: "/data/dreams/x".into(),
            diff_stats: None,
            pid: None,
        }
    }

    #[test]
    fn dream_roundtrip() {
        let db = Db::open_memory().unwrap();
        let mut d = dream("p1", Utc::now(), DreamStatus::Pending);
        d.project.cwd = None;
        db.insert_dream(&d).unwrap();
        assert_eq!(db.get_dream(&d.id).unwrap().unwrap(), d);
        assert!(db.get_dream("drm_missing").unwrap().is_none());

        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.125,
        };
        let stats = DiffStats {
            files_added: 1,
            files_removed: 2,
            files_changed: 3,
        };
        db.set_result(&d.id, &usage, "merged two", &stats).unwrap();
        db.set_pid(&d.id, Some(4242)).unwrap();
        let got = db.get_dream(&d.id).unwrap().unwrap();
        assert_eq!(got.usage, usage);
        assert_eq!(got.summary.as_deref(), Some("merged two"));
        assert_eq!(got.diff_stats, Some(stats));
        assert_eq!(got.pid, Some(4242));
        db.set_pid(&d.id, None).unwrap();
        assert_eq!(db.get_dream(&d.id).unwrap().unwrap().pid, None);
    }

    #[test]
    fn status_update_sets_ended_at_only_for_terminal() {
        let db = Db::open_memory().unwrap();
        let d = dream("p1", Utc::now(), DreamStatus::Pending);
        db.insert_dream(&d).unwrap();
        db.update_status(&d.id, DreamStatus::Running, None).unwrap();
        let got = db.get_dream(&d.id).unwrap().unwrap();
        assert_eq!(got.status, DreamStatus::Running);
        assert!(got.ended_at.is_none());
        db.update_status(&d.id, DreamStatus::Failed, Some("boom"))
            .unwrap();
        let got = db.get_dream(&d.id).unwrap().unwrap();
        assert_eq!(got.status, DreamStatus::Failed);
        assert_eq!(got.error.as_deref(), Some("boom"));
        assert!(got.ended_at.is_some());
        assert!(db
            .update_status("drm_missing", DreamStatus::Failed, None)
            .is_err());
    }

    #[test]
    fn list_filters_by_status_newest_first() {
        let db = Db::open_memory().unwrap();
        let now = Utc::now();
        let a = dream("p1", now - Duration::hours(2), DreamStatus::Completed);
        let b = dream("p2", now - Duration::hours(1), DreamStatus::Failed);
        let c = dream("p1", now, DreamStatus::Completed);
        for d in [&b, &a, &c] {
            db.insert_dream(d).unwrap();
        }
        let ids = |v: Vec<Dream>| v.into_iter().map(|d| d.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.list_dreams(None).unwrap()),
            vec![c.id.clone(), b.id.clone(), a.id.clone()]
        );
        assert_eq!(
            ids(db.list_dreams(Some(DreamStatus::Completed)).unwrap()),
            vec![c.id.clone(), a.id.clone()]
        );
        assert!(db
            .list_dreams(Some(DreamStatus::Applied))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sessions_roundtrip() {
        let db = Db::open_memory().unwrap();
        let d = dream("p1", Utc::now(), DreamStatus::Running);
        db.insert_dream(&d).unwrap();
        let mut s1 = SessionRecord {
            session_id: "s1".into(),
            transcript_path: "/cfg/projects/p1/s1.jsonl".into(),
            status: SessionStatus::Pending,
            findings_path: None,
            usage: Usage::default(),
            error: None,
            duration_ms: 0,
        };
        let s2 = SessionRecord {
            session_id: "s2".into(),
            ..s1.clone()
        };
        db.insert_session(&d.id, &s1).unwrap();
        db.insert_session(&d.id, &s2).unwrap();
        s1.status = SessionStatus::Ok;
        s1.findings_path = Some("/data/f/s1.json".into());
        s1.usage = Usage {
            input_tokens: 7,
            output_tokens: 3,
            cost_usd: 0.01,
        };
        s1.duration_ms = 1234;
        db.update_session(&d.id, &s1).unwrap();
        assert_eq!(db.list_sessions(&d.id).unwrap(), vec![s1, s2]);
        assert!(db.list_sessions("drm_other").unwrap().is_empty());

        assert_eq!(db.get_apply(&d.id).unwrap(), None);
        db.record_apply(&d.id, Path::new("/data/dreams/x/backup"))
            .unwrap();
        assert_eq!(
            db.get_apply(&d.id).unwrap(),
            Some(PathBuf::from("/data/dreams/x/backup"))
        );
    }

    #[test]
    fn last_dreamed_at_none_then_set() {
        let db = Db::open_memory().unwrap();
        let p = project("p1");
        db.upsert_project(&p).unwrap();
        assert_eq!(db.last_dreamed_at(&p).unwrap(), None);
        let t = Utc::now();
        db.set_last_dreamed_at(&p, t).unwrap();
        assert_eq!(db.last_dreamed_at(&p).unwrap(), Some(t));
        // Re-upserting (e.g. via insert_dream) must not reset it.
        db.upsert_project(&p).unwrap();
        db.insert_dream(&dream("p1", t, DreamStatus::Pending))
            .unwrap();
        assert_eq!(db.last_dreamed_at(&p).unwrap(), Some(t));
        // Unknown project: None, and set creates it.
        let q = project("p2");
        assert_eq!(db.last_dreamed_at(&q).unwrap(), None);
        db.set_last_dreamed_at(&q, t).unwrap();
        assert_eq!(db.last_dreamed_at(&q).unwrap(), Some(t));
    }

    #[test]
    fn open_creates_file_and_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dreamd.db");
        let db = Db::open(&path).unwrap();
        db.insert_dream(&dream("p1", Utc::now(), DreamStatus::Pending))
            .unwrap();
        drop(db);
        assert!(path.exists());
        assert_eq!(Db::open(&path).unwrap().list_dreams(None).unwrap().len(), 1);
    }

    #[test]
    fn db_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Db>();
    }
}
