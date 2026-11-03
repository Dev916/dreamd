//! Per-project dream lock: `<data_dir>/locks/<sha256(project key)[..16]>.lock`,
//! created exclusively and holding the owner's pid as text. A lock whose pid
//! is dead (or unreadable) is stale and reclaimed. The lock also tells the
//! orphan sweep whether a `pending`/`running` dream still has a live owner.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::model::Project;

/// A held project lock; removed on drop (only while it still holds our pid).
#[derive(Debug)]
pub struct ProjectLock {
    path: PathBuf,
}

#[derive(Debug)]
pub enum LockError {
    /// Another live process holds the lock.
    Held {
        pid: u32,
        path: PathBuf,
    },
    Io(std::io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held { pid, .. } => {
                write!(f, "project is already being dreamed by pid {pid}")
            }
            LockError::Io(e) => write!(f, "project lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<std::io::Error> for LockError {
    fn from(e: std::io::Error) -> LockError {
        LockError::Io(e)
    }
}

/// `<data_dir>/locks`.
pub fn locks_dir(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("locks")
}

/// The lock file of a project.
pub fn lock_path(cfg: &Config, project: &Project) -> PathBuf {
    let hash = Sha256::digest(project.key().as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    locks_dir(cfg).join(format!("{}.lock", &hex[..16]))
}

/// Whether a process with this pid exists (EPERM counts as alive: it
/// exists but belongs to another user). pid 0 is never alive.
pub fn pid_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if p <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let rc = unsafe { libc::kill(p, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_pid(path: &Path) -> std::io::Result<Option<u32>> {
    let s = std::fs::read_to_string(path)?;
    Ok(s.trim().parse::<u32>().ok())
}

/// The live pid holding the project's lock, if any (None when the lock is
/// absent, unreadable or held by a dead pid).
pub fn holder(cfg: &Config, project: &Project) -> Option<u32> {
    match read_pid(&lock_path(cfg, project)) {
        Ok(Some(pid)) if pid_alive(pid) => Some(pid),
        _ => None,
    }
}

impl ProjectLock {
    /// Take the project's lock, reclaiming it once if its holder is dead.
    pub fn acquire(cfg: &Config, project: &Project) -> Result<ProjectLock, LockError> {
        let path = lock_path(cfg, project);
        std::fs::create_dir_all(locks_dir(cfg))?;
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(std::process::id().to_string().as_bytes())?;
                    f.sync_all()?;
                    return Ok(ProjectLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let pid = match read_pid(&path) {
                        Ok(p) => p,
                        // Removed between our create and read: just retry.
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e.into()),
                    };
                    match pid {
                        Some(pid) if pid_alive(pid) => {
                            return Err(LockError::Held { pid, path });
                        }
                        _ if attempt == 0 => {
                            tracing::warn!("reclaiming stale lock {}", path.display());
                            match std::fs::remove_file(&path) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                Err(e) => return Err(e.into()),
                            }
                        }
                        _ => break,
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(LockError::Io(std::io::Error::other(format!(
            "could not take {} after reclaiming it",
            path.display()
        ))))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if let Ok(Some(pid)) = read_pid(&self.path) {
            if pid == std::process::id() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Config, Project) {
        let t = tempfile::TempDir::new().unwrap();
        let cfg = Config {
            data_dir: t.path().join("data"),
            ..Config::default()
        };
        let p = Project {
            config_dir: t.path().join("claude"),
            slug: "-tmp-proj".into(),
            cwd: None,
            memory_dir: t.path().join("mem"),
        };
        (t, cfg, p)
    }

    /// A pid that is certainly dead: a reaped child.
    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn acquire_writes_pid_and_drop_removes() {
        let (_t, cfg, p) = setup();
        let path = lock_path(&cfg, &p);
        assert!(path.starts_with(cfg.data_dir.join("locks")));
        assert_eq!(path.file_stem().unwrap().len(), 16);
        let lock = ProjectLock::acquire(&cfg, &p).unwrap();
        assert_eq!(lock.path(), path);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        assert_eq!(holder(&cfg, &p), Some(std::process::id()));
        drop(lock);
        assert!(!path.exists());
        assert_eq!(holder(&cfg, &p), None);
    }

    #[test]
    fn second_acquire_is_held() {
        let (_t, cfg, p) = setup();
        let _lock = ProjectLock::acquire(&cfg, &p).unwrap();
        let err = ProjectLock::acquire(&cfg, &p).unwrap_err();
        match &err {
            LockError::Held { pid, .. } => assert_eq!(*pid, std::process::id()),
            e => panic!("{e:?}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "project is already being dreamed by pid {}",
                std::process::id()
            )
        );
    }

    #[test]
    fn other_projects_do_not_conflict() {
        let (_t, cfg, p) = setup();
        let q = Project {
            slug: "-other".into(),
            ..p.clone()
        };
        let _a = ProjectLock::acquire(&cfg, &p).unwrap();
        let _b = ProjectLock::acquire(&cfg, &q).unwrap();
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let (_t, cfg, p) = setup();
        let path = lock_path(&cfg, &p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, dead_pid().to_string()).unwrap();
        assert_eq!(holder(&cfg, &p), None);
        let lock = ProjectLock::acquire(&cfg, &p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        drop(lock);
        // Garbage content is stale too.
        std::fs::write(&path, "not a pid").unwrap();
        let _lock = ProjectLock::acquire(&cfg, &p).unwrap();
    }

    #[test]
    fn drop_removes_only_own_lock() {
        let (_t, cfg, p) = setup();
        let lock = ProjectLock::acquire(&cfg, &p).unwrap();
        let path = lock.path().to_path_buf();
        // Someone else reclaimed and rewrote it.
        std::fs::write(&path, "1").unwrap();
        drop(lock);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1");
    }

    #[test]
    fn pid_alive_basics() {
        assert!(pid_alive(std::process::id()));
        assert!(pid_alive(1)); // launchd/init: EPERM counts as alive
        assert!(!pid_alive(0));
        assert!(!pid_alive(dead_pid()));
        assert!(!pid_alive(u32::MAX));
    }
}
