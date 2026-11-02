//! `dreamd digest` (hidden debug command): Task 4 acceptance criteria.

mod common;

use std::path::PathBuf;

fn transcript(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts")
        .join(name)
}

fn longest_base64_run(s: &str) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

#[test]
fn digest_s1_has_roles_tools_and_errors() {
    let f = common::fixture();
    let out = common::cmd(&f)
        .args(["digest"])
        .arg(transcript("s1.jsonl"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().next(), Some("# session s1"));
    assert!(text.lines().any(|l| l.starts_with("USER:")));
    assert!(text.lines().any(|l| l.starts_with("ASSISTANT:")));
    assert!(text.lines().any(|l| l.starts_with("TOOL Bash: ls")));
    assert!(text.lines().any(|l| l.starts_with("TOOL_ERROR:")));
    assert!(!text.contains("progress"));
    assert!(!text.contains("running..."));
}

#[test]
fn digest_bad_skips_malformed_and_strips_binary() {
    let f = common::fixture();
    let out = common::cmd(&f)
        .args(["digest"])
        .arg(transcript("bad.jsonl"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("skipped 1 malformed line"), "{stderr}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("data:image"));
    assert!(longest_base64_run(&text) <= 100);
    assert!(text.contains("It shows a build error"));
}

#[test]
fn digest_max_bytes_elides_big_transcript() {
    let f = common::fixture();
    let big = transcript("big.jsonl");
    assert!(std::fs::metadata(&big).unwrap().len() >= 20_000);
    let out = common::cmd(&f)
        .args(["digest", "--max-bytes", "2000"])
        .arg(&big)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.len() <= 2100, "{} bytes", out.stdout.len());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("[... elided"));
    assert!(text.starts_with("# session big\n"));
}
