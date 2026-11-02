//! `dreamd doctor`: environment checks.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Run every check. Never short-circuits: each check reports independently.
pub fn run(cfg: &Config) -> Vec<Check> {
    vec![
        check_claude(&cfg.claude_bin),
        check_permissions(&cfg.claude_bin),
        check_config_dirs(&cfg.config_dirs),
        check_data_dir(&cfg.data_dir),
        check_database(&cfg.db_path()),
        check_api_token(&cfg.data_dir),
    ]
}

pub use crate::claude::resolve_bin;

fn check_claude(bin: &str) -> Check {
    match resolve_bin(bin) {
        Some(p) => Check {
            name: "claude binary",
            ok: true,
            detail: p.display().to_string(),
        },
        None => Check {
            name: "claude binary",
            ok: false,
            detail: format!("`{bin}` not found or not executable"),
        },
    }
}

/// FAIL when `claude_bin` is a wrapper that injects the bypass flag.
fn check_permissions(bin: &str) -> Check {
    match crate::claude::detect_bypass_wrapper(bin) {
        Some(p) => Check {
            name: "claude permissions",
            ok: false,
            detail: crate::claude::bypass_wrapper_error(&p),
        },
        None => Check {
            name: "claude permissions",
            ok: true,
            detail: "permission modes are honoured (no bypass wrapper)".into(),
        },
    }
}

fn check_config_dirs(dirs: &[PathBuf]) -> Check {
    let (present, missing): (Vec<_>, Vec<_>) = dirs.iter().partition(|d| d.is_dir());
    let show = |v: &[&PathBuf]| {
        v.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut detail = format!("{} of {} present", present.len(), dirs.len());
    if !present.is_empty() {
        detail.push_str(&format!(": {}", show(&present)));
    }
    if !missing.is_empty() {
        detail.push_str(&format!("; missing: {}", show(&missing)));
    }
    Check {
        name: "config dirs",
        ok: !present.is_empty(),
        detail,
    }
}

fn check_data_dir(dir: &Path) -> Check {
    let res =
        std::fs::create_dir_all(dir).and_then(|_| tempfile::NamedTempFile::new_in(dir).map(|_| ()));
    Check {
        name: "data dir",
        ok: res.is_ok(),
        detail: match res {
            Ok(()) => format!("{} (writable)", dir.display()),
            Err(e) => format!("{}: {e}", dir.display()),
        },
    }
}

fn check_database(path: &Path) -> Check {
    let res = rusqlite::Connection::open(path)
        .and_then(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)));
    Check {
        name: "database",
        ok: res.is_ok(),
        detail: match res {
            Ok(_) => path.display().to_string(),
            Err(e) => format!("{}: {e}", path.display()),
        },
    }
}

/// ok when the token file is absent (created on first `serve`) or 0600;
/// FAIL when group/other have any access.
fn check_api_token(data_dir: &Path) -> Check {
    let file = crate::token::path(data_dir);
    let (ok, detail) = match crate::token::mode(data_dir) {
        Ok(None) => (
            true,
            format!("{} (absent; created on first serve)", file.display()),
        ),
        Ok(Some(m)) if m & 0o077 == 0 => (true, format!("{} ({m:04o})", file.display())),
        Ok(Some(m)) => (
            false,
            format!(
                "{} ({m:04o}): readable beyond the owner; run `chmod 600` on it",
                file.display()
            ),
        ),
        Err(e) => (false, format!("{}: {e}", file.display())),
    };
    Check {
        name: "api token",
        ok,
        detail,
    }
}

/// Human-readable report, one line per check, prefixed `ok` or `FAIL`.
pub fn render(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            format!(
                "{:<4} {:<18} {}\n",
                if c.ok { "ok" } else { "FAIL" },
                c.name,
                c.detail
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prefixes_ok_and_fail() {
        let out = render(&[
            Check {
                name: "a",
                ok: true,
                detail: "x".into(),
            },
            Check {
                name: "b",
                ok: false,
                detail: "y".into(),
            },
        ]);
        let lines: Vec<_> = out.lines().collect();
        assert!(lines[0].starts_with("ok "));
        assert!(lines[1].starts_with("FAIL "));
    }

    #[test]
    fn config_dirs_ok_if_any_present() {
        let tmp = tempfile::tempdir().unwrap();
        let c = check_config_dirs(&[tmp.path().to_path_buf(), tmp.path().join("nope")]);
        assert!(c.ok);
        assert!(c.detail.contains("missing"));
        assert!(!check_config_dirs(&[tmp.path().join("nope")]).ok);
    }

    #[test]
    fn resolve_bin_rejects_non_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("claude");
        std::fs::write(&f, "#!/bin/sh\n").unwrap();
        assert!(resolve_bin(f.to_str().unwrap()).is_none());
    }
}
