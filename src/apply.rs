//! Apply a completed dream to the live memory dir, and roll it back.
//!
//! Apply order: status check → live hash vs `in.sha256` → copy live →
//! `backup/` → stage `out/` into `<memory_dir>.dreamd-new` (a sibling, so the
//! same filesystem) → `rename(live, <memory_dir>.dreamd-old)` →
//! `rename(new, live)` → remove old. A crash before the first rename leaves
//! the live dir untouched; a crash between the renames leaves the original
//! intact at `.dreamd-old`, which the next apply/rollback moves back before
//! doing anything else. The live path never holds a half-written copy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::dream::{dir_of, Ctx};
use crate::fsops::{copy_dir, hash_dir};
use crate::model::{Dream, DreamStatus};

/// What an apply or rollback touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub dream_id: String,
    /// The pre-apply copy of the live memory dir (`<dream_dir>/backup`).
    pub backup_path: PathBuf,
    /// The live memory dir that was swapped.
    pub memory_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The dream is not in a state this operation accepts.
    #[error("dream is {0}")]
    BadStatus(DreamStatus),
    /// The live memory dir changed since the dream's snapshot.
    #[error("memory changed since snapshot (rerun the dream or use --force)")]
    HashMismatch,
    #[error("dream not found")]
    NotFound,
    #[error("{0}")]
    Io(#[from] io::Error),
}

fn db_err(e: anyhow::Error) -> ApplyError {
    ApplyError::Io(io::Error::other(format!("{e:#}")))
}

fn ctx_io<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> Result<T, ApplyError> {
    r.map_err(|e| ApplyError::Io(io::Error::new(e.kind(), format!("{}: {e}", what()))))
}

/// `<parent>/<name>.<suffix>`: a sibling of `dir` on the same filesystem.
fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "memory".into());
    dir.with_file_name(format!("{name}.{suffix}"))
}

/// Undo the leftovers of an interrupted swap: if the live dir is missing but
/// `.dreamd-old` exists, move it back; drop a stale `.dreamd-new`; drop a
/// stale `.dreamd-old` once the live dir is in place.
fn recover_interrupted(live: &Path) -> io::Result<()> {
    let old = sibling(live, "dreamd-old");
    let new = sibling(live, "dreamd-new");
    if !live.exists() && old.is_dir() {
        fs::rename(&old, live)?;
    }
    if new.exists() {
        fs::remove_dir_all(&new)?;
    }
    if live.exists() && old.exists() {
        fs::remove_dir_all(&old)?;
    }
    Ok(())
}

/// Replace `live` with a copy of `src` via stage + two renames.
fn swap_in(src: &Path, live: &Path) -> Result<(), ApplyError> {
    let new = sibling(live, "dreamd-new");
    let old = sibling(live, "dreamd-old");
    if let Err(e) = copy_dir(src, &new) {
        let _ = fs::remove_dir_all(&new);
        return ctx_io(Err(e), || format!("stage {}", new.display()));
    }
    ctx_io(fs::rename(live, &old), || {
        format!("rename {} → {}", live.display(), old.display())
    })?;
    if let Err(e) = fs::rename(&new, live) {
        // Put the original back; the staged copy is disposable.
        let _ = fs::rename(&old, live);
        let _ = fs::remove_dir_all(&new);
        return ctx_io(Err(e), || {
            format!("rename {} → {}", new.display(), live.display())
        });
    }
    ctx_io(fs::remove_dir_all(&old), || {
        format!("remove {}", old.display())
    })
}

fn load(ctx: &Ctx, dream_id: &str) -> Result<Dream, ApplyError> {
    ctx.db
        .get_dream(dream_id)
        .map_err(db_err)?
        .ok_or(ApplyError::NotFound)
}

/// Apply a completed dream. `DREAMD_FAIL_AFTER_BACKUP=1` (test hook) aborts
/// right after the backup is written, before anything is staged.
pub fn apply(ctx: &Ctx, dream_id: &str, force: bool) -> Result<ApplyReport, ApplyError> {
    let fail_after_backup = std::env::var("DREAMD_FAIL_AFTER_BACKUP").is_ok_and(|v| v == "1");
    apply_with(ctx, dream_id, force, fail_after_backup)
}

fn apply_with(
    ctx: &Ctx,
    dream_id: &str,
    force: bool,
    fail_after_backup: bool,
) -> Result<ApplyReport, ApplyError> {
    let d = load(ctx, dream_id)?;
    if d.status != DreamStatus::Completed {
        return Err(ApplyError::BadStatus(d.status));
    }
    let dir = dir_of(&d);
    let live = d.project.memory_dir.clone();
    ctx_io(recover_interrupted(&live), || {
        format!("recover {}", live.display())
    })?;

    if !force {
        let want = ctx_io(fs::read_to_string(dir.join("in.sha256")), || {
            format!("read {}", dir.join("in.sha256").display())
        })?;
        let have = ctx_io(hash_dir(&live), || format!("hash {}", live.display()))?;
        if have != want.trim() {
            return Err(ApplyError::HashMismatch);
        }
    }

    let backup = dir.join("backup");
    if backup.exists() {
        ctx_io(fs::remove_dir_all(&backup), || {
            format!("remove stale {}", backup.display())
        })?;
    }
    ctx_io(copy_dir(&live, &backup), || {
        format!("backup {} → {}", live.display(), backup.display())
    })?;
    if fail_after_backup {
        return Err(ApplyError::Io(io::Error::other(
            "aborted after backup (DREAMD_FAIL_AFTER_BACKUP)",
        )));
    }

    swap_in(&d.output_dir, &live)?;

    ctx.db.record_apply(dream_id, &backup).map_err(db_err)?;
    ctx.db
        .set_last_dreamed_at(&d.project, Utc::now())
        .map_err(db_err)?;
    ctx.db
        .update_status(dream_id, DreamStatus::Applied, None)
        .map_err(db_err)?;
    Ok(ApplyReport {
        dream_id: dream_id.to_string(),
        backup_path: backup,
        memory_dir: live,
    })
}

/// Restore an applied dream's backup into the live memory dir.
pub fn rollback(ctx: &Ctx, dream_id: &str) -> Result<ApplyReport, ApplyError> {
    let d = load(ctx, dream_id)?;
    if d.status != DreamStatus::Applied {
        return Err(ApplyError::BadStatus(d.status));
    }
    let backup = ctx
        .db
        .get_apply(dream_id)
        .map_err(db_err)?
        .unwrap_or_else(|| dir_of(&d).join("backup"));
    if !backup.is_dir() {
        return Err(ApplyError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("backup missing: {}", backup.display()),
        )));
    }
    let live = d.project.memory_dir.clone();
    ctx_io(recover_interrupted(&live), || {
        format!("recover {}", live.display())
    })?;
    swap_in(&backup, &live)?;
    ctx.db
        .update_status(dream_id, DreamStatus::RolledBack, None)
        .map_err(db_err)?;
    Ok(ApplyReport {
        dream_id: dream_id.to_string(),
        backup_path: backup,
        memory_dir: live,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::claude::CliClaude;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Project, Usage};
    use tempfile::TempDir;

    struct T {
        _tmp: TempDir,
        ctx: Ctx,
        live: PathBuf,
        dir: PathBuf,
        id: String,
    }

    fn write(dir: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for (n, c) in files {
            fs::write(dir.join(n), c).unwrap();
        }
    }

    /// A completed dream on a tempdir: live = in/ = {a, b}, out/ = {a', c}.
    fn setup() -> T {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        let ctx = Ctx {
            cfg,
            db: Arc::new(Db::open_memory().unwrap()),
            claude: Arc::new(CliClaude {
                bin: "false".into(),
            }),
        };
        let live = tmp.path().join("cfg/projects/p/memory");
        write(&live, &[("a.md", "a\n"), ("b.md", "b\n")]);
        let id = Dream::new_id();
        let dir = ctx.cfg.dreams_dir().join(&id);
        copy_dir(&live, &dir.join("in")).unwrap();
        write(&dir.join("out"), &[("a.md", "a2\n"), ("c.md", "c\n")]);
        let h = hash_dir(&dir.join("in")).unwrap();
        fs::write(dir.join("in.sha256"), format!("{h}\n")).unwrap();
        let d = Dream {
            id: id.clone(),
            project: Project {
                config_dir: tmp.path().join("cfg"),
                slug: "p".into(),
                cwd: None,
                memory_dir: live.clone(),
            },
            status: DreamStatus::Completed,
            model: "opus".into(),
            instructions: None,
            session_ids: vec!["s1".into()],
            created_at: Utc::now(),
            ended_at: None,
            usage: Usage::default(),
            summary: None,
            error: None,
            output_dir: dir.join("out"),
            diff_stats: None,
            pid: None,
            archived_at: None,
        };
        ctx.db.insert_dream(&d).unwrap();
        T {
            _tmp: tmp,
            ctx,
            live,
            dir,
            id,
        }
    }

    fn h(p: &Path) -> String {
        hash_dir(p).unwrap()
    }

    fn status(t: &T) -> DreamStatus {
        t.ctx.db.get_dream(&t.id).unwrap().unwrap().status
    }

    fn no_siblings(live: &Path) {
        assert!(!sibling(live, "dreamd-new").exists());
        assert!(!sibling(live, "dreamd-old").exists());
    }

    #[test]
    fn apply_swaps_and_backs_up() {
        let t = setup();
        let r = apply_with(&t.ctx, &t.id, false, false).unwrap();
        assert_eq!(r.backup_path, t.dir.join("backup"));
        assert_eq!(r.memory_dir, t.live);
        assert_eq!(h(&t.live), h(&t.dir.join("out")));
        assert_eq!(h(&r.backup_path), h(&t.dir.join("in")));
        assert_eq!(status(&t), DreamStatus::Applied);
        assert_eq!(t.ctx.db.get_apply(&t.id).unwrap(), Some(r.backup_path));
        let p = t.ctx.db.get_dream(&t.id).unwrap().unwrap().project;
        assert!(t.ctx.db.last_dreamed_at(&p).unwrap().is_some());
        no_siblings(&t.live);
    }

    #[test]
    fn apply_refuses_hash_mismatch() {
        let t = setup();
        fs::write(t.live.join("b.md"), "changed\n").unwrap();
        let before = h(&t.live);
        let e = apply_with(&t.ctx, &t.id, false, false).unwrap_err();
        assert!(matches!(e, ApplyError::HashMismatch), "{e}");
        assert_eq!(h(&t.live), before);
        assert!(!t.dir.join("backup").exists());
        assert_eq!(status(&t), DreamStatus::Completed);
    }

    #[test]
    fn apply_force_overrides_mismatch() {
        let t = setup();
        fs::write(t.live.join("b.md"), "changed\n").unwrap();
        let before = h(&t.live);
        let r = apply_with(&t.ctx, &t.id, true, false).unwrap();
        assert_eq!(h(&t.live), h(&t.dir.join("out")));
        assert_eq!(h(&r.backup_path), before);
    }

    #[test]
    fn apply_refuses_bad_status() {
        let t = setup();
        for s in [
            DreamStatus::Failed,
            DreamStatus::Running,
            DreamStatus::Applied,
        ] {
            t.ctx.db.update_status(&t.id, s, None).unwrap();
            let e = apply_with(&t.ctx, &t.id, true, false).unwrap_err();
            assert!(matches!(e, ApplyError::BadStatus(x) if x == s), "{e}");
            assert_eq!(e.to_string(), format!("dream is {s}"));
        }
        assert_eq!(h(&t.live), h(&t.dir.join("in")));
        assert!(matches!(
            apply_with(&t.ctx, "drm_nope", false, false),
            Err(ApplyError::NotFound)
        ));
    }

    #[test]
    fn rollback_restores_input() {
        let t = setup();
        assert!(matches!(
            rollback(&t.ctx, &t.id),
            Err(ApplyError::BadStatus(DreamStatus::Completed))
        ));
        apply_with(&t.ctx, &t.id, false, false).unwrap();
        let r = rollback(&t.ctx, &t.id).unwrap();
        assert_eq!(h(&t.live), h(&t.dir.join("in")));
        assert!(r.backup_path.is_dir(), "backup kept");
        assert_eq!(status(&t), DreamStatus::RolledBack);
        no_siblings(&t.live);
    }

    #[test]
    fn abort_after_backup_leaves_live_intact() {
        let t = setup();
        let e = apply_with(&t.ctx, &t.id, false, true).unwrap_err();
        assert!(matches!(e, ApplyError::Io(_)), "{e}");
        assert_eq!(h(&t.live), h(&t.dir.join("in")));
        assert!(t.dir.join("backup").is_dir());
        assert_eq!(status(&t), DreamStatus::Completed);
        no_siblings(&t.live);
        // Retry succeeds (stale backup replaced).
        apply_with(&t.ctx, &t.id, false, false).unwrap();
        assert_eq!(h(&t.live), h(&t.dir.join("out")));
    }

    #[test]
    fn interrupted_swap_is_recovered() {
        let t = setup();
        // Crash between the two renames: live moved aside, new staged.
        let old = sibling(&t.live, "dreamd-old");
        let new = sibling(&t.live, "dreamd-new");
        fs::rename(&t.live, &old).unwrap();
        copy_dir(&t.dir.join("out"), &new).unwrap();
        apply_with(&t.ctx, &t.id, false, false).unwrap();
        assert_eq!(h(&t.live), h(&t.dir.join("out")));
        assert_eq!(h(&t.dir.join("backup")), h(&t.dir.join("in")));
        no_siblings(&t.live);
    }
}
