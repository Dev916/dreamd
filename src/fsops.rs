//! Filesystem helpers for dream snapshots: recursive copy, content hash,
//! unified diff between two directory trees.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::model::DiffStats;

/// One regular file in a tree: its `/`-joined relative path and the path to read it from.
struct Entry {
    rel: String,
    path: PathBuf,
}

/// Files under `root`, sorted by relative path. Symlinks are followed only when
/// they resolve to a regular file inside `root`; anything else (escaping,
/// dangling, or pointing at a directory) is skipped.
fn files(root: &Path) -> io::Result<Vec<Entry>> {
    let canon_root = fs::canonicalize(root)?;
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let e = e.map_err(io::Error::other)?;
        let ft = e.file_type();
        let include = if ft.is_file() {
            true
        } else if ft.is_symlink() {
            match fs::canonicalize(e.path()) {
                Ok(t) => t.starts_with(&canon_root) && t.is_file(),
                Err(_) => false,
            }
        } else {
            false
        };
        if !include {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(Entry {
            rel,
            path: e.path().to_path_buf(),
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Recursively copy `src` into `dst` (created if missing), preserving relative
/// paths. Symlinks pointing outside `src` are not followed.
pub fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in walkdir::WalkDir::new(src).follow_links(false).min_depth(1) {
        let e = e.map_err(io::Error::other)?;
        if e.file_type().is_dir() {
            let rel = e.path().strip_prefix(src).map_err(io::Error::other)?;
            fs::create_dir_all(dst.join(rel))?;
        }
    }
    for f in files(src)? {
        let to = dst.join(&f.rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&f.path, &to)?;
    }
    Ok(())
}

/// Hex sha256 over every file's relative path and contents, in sorted path order.
pub fn hash_dir(dir: &Path) -> io::Result<String> {
    let mut h = Sha256::new();
    for f in files(dir)? {
        let data = fs::read(&f.path)?;
        h.update(f.rel.as_bytes());
        h.update([0u8]);
        h.update((data.len() as u64).to_le_bytes());
        h.update(&data);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Unified diff `a` → `b`, one section per changed file with headers
/// `--- a/<rel>` / `+++ b/<rel>`; added/removed files diff against empty.
pub fn unified_diff(a: &Path, b: &Path) -> io::Result<(String, DiffStats)> {
    let fa = files(a)?;
    let fb = files(b)?;
    let rels: BTreeSet<&str> = fa.iter().chain(fb.iter()).map(|f| f.rel.as_str()).collect();
    let read = |set: &[Entry], rel: &str| -> io::Result<Option<String>> {
        match set.iter().find(|f| f.rel == rel) {
            Some(f) => Ok(Some(
                String::from_utf8_lossy(&fs::read(&f.path)?).into_owned(),
            )),
            None => Ok(None),
        }
    };
    let mut out = String::new();
    let mut stats = DiffStats::default();
    for rel in rels {
        let old = read(&fa, rel)?;
        let new = read(&fb, rel)?;
        match (&old, &new) {
            (Some(o), Some(n)) if o == n => continue,
            (Some(_), Some(_)) => stats.files_changed += 1,
            (None, Some(_)) => stats.files_added += 1,
            (Some(_), None) => stats.files_removed += 1,
            (None, None) => continue,
        }
        let o = old.unwrap_or_default();
        let n = new.unwrap_or_default();
        let diff = TextDiff::from_lines(&o, &n);
        out.push_str(
            &diff
                .unified_diff()
                .context_radius(3)
                .header(&format!("a/{rel}"), &format!("b/{rel}"))
                .to_string(),
        );
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn copy_dir_roundtrip_hash_equal() {
        let t = tempfile::TempDir::new().unwrap();
        let src = t.path().join("src");
        write(&src.join("MEMORY.md"), "- [A](a.md) — a\n");
        write(
            &src.join("a.md"),
            "---\nname: a\ndescription: a\n---\nbody\n",
        );
        write(&src.join("sub/deep.md"), "deep\n");
        let secret = t.path().join("secret.txt");
        write(&secret, "outside\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, src.join("escape.md")).unwrap();
            std::os::unix::fs::symlink(src.join("a.md"), src.join("alias.md")).unwrap();
        }
        let dst = t.path().join("nested/dst");
        copy_dir(&src, &dst).unwrap();
        assert!(dst.join("sub/deep.md").is_file());
        assert!(!dst.join("escape.md").exists(), "escaping symlink copied");
        assert_eq!(hash_dir(&src).unwrap(), hash_dir(&dst).unwrap());
        assert_eq!(hash_dir(&src).unwrap(), hash_dir(&src).unwrap());
        assert_eq!(hash_dir(&src).unwrap().len(), 64);
    }

    #[test]
    fn hash_changes_on_content_change() {
        let t = tempfile::TempDir::new().unwrap();
        write(&t.path().join("a.md"), "one\n");
        let h1 = hash_dir(t.path()).unwrap();
        write(&t.path().join("a.md"), "two\n");
        let h2 = hash_dir(t.path()).unwrap();
        assert_ne!(h1, h2);
        fs::rename(t.path().join("a.md"), t.path().join("b.md")).unwrap();
        assert_ne!(h2, hash_dir(t.path()).unwrap(), "rename must change hash");
    }

    #[test]
    fn unified_diff_counts_added_removed_changed() {
        let t = tempfile::TempDir::new().unwrap();
        let a = t.path().join("a");
        let b = t.path().join("b");
        write(&a.join("MEMORY.md"), "- one\n");
        write(&a.join("same.md"), "same\n");
        write(&a.join("gone.md"), "bye\n");
        write(&b.join("MEMORY.md"), "- one\n- two\n");
        write(&b.join("same.md"), "same\n");
        write(&b.join("new.md"), "hello\n");
        let (text, stats) = unified_diff(&a, &b).unwrap();
        assert_eq!(
            stats,
            DiffStats {
                files_added: 1,
                files_removed: 1,
                files_changed: 1
            }
        );
        assert!(
            text.starts_with("--- a/MEMORY.md\n+++ b/MEMORY.md\n"),
            "{text}"
        );
        assert!(text.contains("--- a/new.md\n+++ b/new.md\n"));
        assert!(text.contains("+hello"));
        assert!(text.contains("--- a/gone.md\n+++ b/gone.md\n"));
        assert!(text.contains("-bye"));
        assert!(!text.contains("same.md"));
    }
}
