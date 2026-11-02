//! `dreamd postcheck <in> <out>` (hidden): Task 7 acceptance criteria.

mod common;

use std::path::Path;

use predicates::prelude::*;

const GOOD: &str = "---\nname: x\ndescription: y\nmetadata:\n  type: project\n---\nbody\n";

/// `in/` and `out/` both a copy of the fixture memory dir.
fn setup(f: &common::Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    let i = f.root.path().join("dream/in");
    let o = f.root.path().join("dream/out");
    for d in [&i, &o] {
        std::fs::create_dir_all(d).unwrap();
        for e in std::fs::read_dir(&f.memory_dir).unwrap() {
            let p = e.unwrap().path();
            std::fs::copy(&p, d.join(p.file_name().unwrap())).unwrap();
        }
    }
    (i, o)
}

fn append(p: &Path, s: &str) {
    let mut t = std::fs::read_to_string(p).unwrap();
    t.push_str(s);
    std::fs::write(p, t).unwrap();
}

#[test]
fn valid_addition_exits_0_with_stats() {
    let f = common::fixture();
    let (i, o) = setup(&f);
    std::fs::write(o.join("fact-c.md"), GOOD).unwrap();
    append(&o.join("MEMORY.md"), "- [Fact C](fact-c.md) — new\n");
    common::cmd(&f)
        .arg("postcheck")
        .arg(&i)
        .arg(&o)
        .assert()
        .code(0)
        .stdout("ok: 4 files, +1 -0 ~1\n");
}

#[test]
fn new_missing_frontmatter_exits_4() {
    let f = common::fixture();
    let (i, o) = setup(&f);
    std::fs::write(o.join("notes.md"), "just notes\n").unwrap();
    common::cmd(&f)
        .arg("postcheck")
        .arg(&i)
        .arg(&o)
        .assert()
        .code(4)
        .stdout(predicate::str::contains(
            "violation: missing frontmatter: notes.md",
        ));
}

#[test]
fn preexisting_missing_frontmatter_is_ignored() {
    let f = common::fixture();
    let (i, o) = setup(&f);
    std::fs::write(i.join("notes.md"), "just notes\n").unwrap();
    std::fs::write(o.join("notes.md"), "just notes\n").unwrap();
    common::cmd(&f)
        .arg("postcheck")
        .arg(&i)
        .arg(&o)
        .assert()
        .code(0)
        .stdout(predicate::str::starts_with("ok: "));
}

#[test]
fn broken_index_link_exits_4() {
    let f = common::fixture();
    let (i, o) = setup(&f);
    append(&o.join("MEMORY.md"), "- [Gone](gone.md) — missing\n");
    common::cmd(&f)
        .arg("postcheck")
        .arg(&i)
        .arg(&o)
        .assert()
        .code(4)
        .stdout(predicate::str::contains(
            "violation: MEMORY.md links to missing file: gone.md",
        ));
}
