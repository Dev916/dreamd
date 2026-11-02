//! Post-check of an aggregator's `out/` dir: memory-file conventions,
//! `MEMORY.md` link integrity (new violations only), and writes that
//! escaped `out/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// A convention violation introduced by a dream.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Violation {
    /// A memory `.md` (relative path) without `---` frontmatter carrying `name:` and `description:`.
    MissingFrontmatter(String),
    /// A `MEMORY.md` link whose target file does not exist.
    BrokenIndexLink { target: String },
    /// A file outside `out/` under the dream dir was created, modified or removed.
    EscapedWrite(PathBuf),
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::MissingFrontmatter(p) => write!(f, "missing frontmatter: {p}"),
            Violation::BrokenIndexLink { target } => {
                write!(f, "MEMORY.md links to missing file: {target}")
            }
            Violation::EscapedWrite(p) => write!(f, "write outside out/: {}", p.display()),
        }
    }
}

/// True when `text` starts with `---\n` and has `name:` and `description:`
/// lines before the closing `---`.
fn has_frontmatter(text: &str) -> bool {
    let text = text.replace("\r\n", "\n");
    let Some(rest) = text.strip_prefix("---\n") else {
        return false;
    };
    let (mut name, mut desc) = (false, false);
    for line in rest.lines() {
        if line.trim_end() == "---" {
            return name && desc;
        }
        name |= line.starts_with("name:");
        desc |= line.starts_with("description:");
    }
    false
}

/// Link targets in a `MEMORY.md`: the `(...)` after every `](`. URLs and
/// pure anchors are ignored; `#fragment`s are stripped.
fn index_links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("](") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find(')') else { break };
        let raw = rest[..end].trim();
        rest = &rest[end + 1..];
        let target = raw.split('#').next().unwrap_or("").trim();
        if target.is_empty() || target.contains("://") || target.starts_with("mailto:") {
            continue;
        }
        out.push(target.to_string());
    }
    out
}

/// Every violation present in one memory dir.
fn violations(dir: &Path) -> BTreeSet<Violation> {
    let mut out = BTreeSet::new();
    for e in walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if !e.file_type().is_file() {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(dir) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if !rel.ends_with(".md") {
            continue;
        }
        let text = String::from_utf8_lossy(&fs::read(e.path()).unwrap_or_default()).into_owned();
        if rel == "MEMORY.md" {
            for target in index_links(&text) {
                if !dir.join(&target).is_file() {
                    out.insert(Violation::BrokenIndexLink { target });
                }
            }
        } else if !has_frontmatter(&text) {
            out.insert(Violation::MissingFrontmatter(rel));
        }
    }
    out
}

/// Violations in `out_dir` that were not already present in `in_dir`.
pub fn check(in_dir: &Path, out_dir: &Path) -> Vec<Violation> {
    let before = violations(in_dir);
    violations(out_dir)
        .into_iter()
        .filter(|v| !before.contains(v))
        .collect()
}

/// mtime (ns since epoch) of every file under `dream_dir` except the `out/` subtree.
pub fn snapshot_mtimes(dream_dir: &Path) -> BTreeMap<PathBuf, u64> {
    let out_dir = dream_dir.join("out");
    let mut m = BTreeMap::new();
    let walk = walkdir::WalkDir::new(dream_dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| e.path() != out_dir);
    for e in walk.flatten() {
        if e.file_type().is_dir() {
            continue;
        }
        let ns = e
            .path()
            .symlink_metadata()
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        m.insert(e.path().to_path_buf(), ns);
    }
    m
}

/// Files outside `out/` that appeared, disappeared or changed mtime between
/// two `snapshot_mtimes` of `dream_dir`.
pub fn check_escapes(
    dream_dir: &Path,
    before: &BTreeMap<PathBuf, u64>,
    after: &BTreeMap<PathBuf, u64>,
) -> Vec<Violation> {
    let out_dir = dream_dir.join("out");
    let keys: BTreeSet<&PathBuf> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|p| !p.starts_with(&out_dir))
        .filter(|p| before.get(*p) != after.get(*p))
        .map(|p| Violation::EscapedWrite(p.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: x\ndescription: y\nmetadata:\n  type: project\n---\nbody\n";

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn pair() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::TempDir::new().unwrap();
        let i = t.path().join("in");
        let o = t.path().join("out");
        for d in [&i, &o] {
            write(&d.join("MEMORY.md"), "- [A](a.md) — a\n");
            write(&d.join("a.md"), GOOD);
        }
        (t, i, o)
    }

    #[test]
    fn frontmatter_rules() {
        assert!(has_frontmatter(GOOD));
        assert!(!has_frontmatter("no frontmatter\n"));
        assert!(!has_frontmatter("---\nname: x\n---\n"));
        assert!(!has_frontmatter("---\nname: x\ndescription: y\n"));
        assert!(!has_frontmatter("---\nname: x\n---\ndescription: y\n"));
    }

    #[test]
    fn index_link_parsing() {
        let l = index_links("- [A](a.md) — x\n- [W](https://e.com/x) y\n- [B](b.md#s) z\n");
        assert_eq!(l, vec!["a.md", "b.md"]);
    }

    #[test]
    fn postcheck_ignores_preexisting_violations() {
        let (_t, i, o) = pair();
        for d in [&i, &o] {
            write(&d.join("notes.md"), "no frontmatter\n");
            write(
                &d.join("MEMORY.md"),
                "- [A](a.md) — a\n- [G](gone.md) — g\n",
            );
        }
        assert_eq!(check(&i, &o), vec![]);
    }

    #[test]
    fn postcheck_flags_new_missing_frontmatter() {
        let (_t, i, o) = pair();
        write(&o.join("notes.md"), "no frontmatter\n");
        let v = check(&i, &o);
        assert_eq!(v, vec![Violation::MissingFrontmatter("notes.md".into())]);
        assert_eq!(v[0].to_string(), "missing frontmatter: notes.md");
    }

    #[test]
    fn postcheck_flags_broken_index_link() {
        let (_t, i, o) = pair();
        write(
            &o.join("MEMORY.md"),
            "- [A](a.md) — a\n- [G](gone.md) — g\n",
        );
        let v = check(&i, &o);
        assert_eq!(
            v,
            vec![Violation::BrokenIndexLink {
                target: "gone.md".into()
            }]
        );
        assert_eq!(v[0].to_string(), "MEMORY.md links to missing file: gone.md");
    }

    #[test]
    fn check_escapes_detects_write_outside_out() {
        let t = tempfile::TempDir::new().unwrap();
        let d = t.path();
        write(&d.join("in/a.md"), "a\n");
        write(&d.join("out/a.md"), "a\n");
        write(&d.join("findings/s1.json"), "{}\n");
        let before = snapshot_mtimes(d);
        assert!(!before.keys().any(|p| p.starts_with(d.join("out"))));
        // Writes inside out/ are fine.
        write(&d.join("out/b.md"), GOOD);
        write(&d.join("out/a.md"), "changed\n");
        assert_eq!(check_escapes(d, &before, &snapshot_mtimes(d)), vec![]);
        // A new file in in/ and a modified finding are escapes.
        write(&d.join("in/evil.md"), "evil\n");
        let f = d.join("findings/s1.json");
        filetime::set_file_mtime(&f, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        let v = check_escapes(d, &before, &snapshot_mtimes(d));
        assert_eq!(
            v,
            vec![
                Violation::EscapedWrite(f.clone()),
                Violation::EscapedWrite(d.join("in/evil.md")),
            ]
        );
        assert!(v[1].to_string().starts_with("write outside out/: "));
    }
}
