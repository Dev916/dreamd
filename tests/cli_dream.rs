//! `dreamd dream` / `show` / `diff` / `summary`: Task 8 acceptance criteria,
//! run end to end against `tests/stub-claude.sh` in `auto` mode.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use predicates::prelude::*;

fn dream(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = common::cmd(f);
    c.env("DREAMD_STUB_MODE", "auto")
        .timeout(Duration::from_secs(60))
        .arg("dream")
        .arg(common::SLUG);
    c
}

/// sha256-free tree fingerprint: sorted (relative path, bytes).
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v: Vec<(PathBuf, Vec<u8>)> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            (
                e.path().strip_prefix(dir).unwrap().to_path_buf(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// The single dream dir under `<data_dir>/dreams/`.
fn only_dream_dir(f: &common::Fixture) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(f.data_dir.join("dreams"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn dream_id(stdout: &str) -> String {
    let i = stdout.find("drm_").expect("dream id in stdout");
    stdout[i..].split_whitespace().next().unwrap().to_string()
}

#[test]
fn forced_dream_completes_and_leaves_live_memory_untouched() {
    let f = common::fixture();
    let before = tree(&f.memory_dir);
    let out = dream(&f)
        .arg("--force")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("completed: 3 sessions mined (")
            && stdout.contains("KB read), +1 -0 ~1 files"),
        "{stdout}"
    );
    let id = dream_id(&stdout);
    let dir = only_dream_dir(&f);
    assert_eq!(dir.file_name().unwrap().to_str().unwrap(), id);
    assert!(
        stdout.contains(&dir.join("out").display().to_string()),
        "{stdout}"
    );
    for p in ["in", "out", "findings"] {
        assert!(dir.join(p).is_dir(), "{p} missing");
    }
    for p in ["aggregate.md", "dream.diff"] {
        assert!(dir.join(p).is_file(), "{p} missing");
    }
    assert_eq!(std::fs::read_dir(dir.join("findings")).unwrap().count(), 3);
    assert_eq!(tree(&f.memory_dir), before, "live memory dir changed");

    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("completed"))
        .stdout(predicate::str::contains("opus"))
        .stdout(predicate::str::contains("sessions     3"))
        .stdout(predicate::str::contains("cost_usd=0.0040"))
        .stdout(predicate::str::contains("Merged 1 insight."));
    let diff = common::cmd(&f)
        .args(["diff", &id])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(diff)
            .unwrap()
            .starts_with("--- a/MEMORY.md"),
        "diff header"
    );
    let summary = common::cmd(&f)
        .args(["summary", &id])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(summary, std::fs::read(dir.join("aggregate.md")).unwrap());
}

#[test]
fn gate_skip_exits_2_with_reason() {
    let f = common::fixture();
    std::fs::write(f.config_file(), "min_sessions = 10\n").unwrap();
    dream(&f)
        .assert()
        .code(2)
        .stdout(predicate::str::contains("need 10 sessions, have 3"));
    assert!(!f.data_dir.join("dreams").exists());
}

#[test]
fn unknown_project_exits_3() {
    let f = common::fixture();
    common::cmd(&f)
        .args(["dream", "nosuchslug", "--force"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("project not found"));
}

#[test]
fn escaped_write_fails_the_dream_and_keeps_out() {
    let f = common::fixture();
    let before = tree(&f.memory_dir);
    // The aggregator runs with cwd = <dream_dir>/out.
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_ESCAPE", "../in/evil.md")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("violation"));
    let dir = only_dream_dir(&f);
    assert!(dir.join("in/evil.md").is_file());
    assert!(dir.join("out/dreamed.md").is_file(), "out/ kept");
    assert_eq!(tree(&f.memory_dir), before);
    let id = dir.file_name().unwrap().to_str().unwrap().to_string();
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("failed"))
        .stdout(predicate::str::contains("violation"));
}

#[test]
fn instructions_reach_the_aggregator_prompt() {
    let f = common::fixture();
    let prompt_out = f.root.path().join("prompt.txt");
    dream(&f)
        .args(["--force", "--instructions", "focus on X"])
        .env("DREAMD_STUB_PROMPT_OUT", &prompt_out)
        .assert()
        .code(0);
    let p = std::fs::read_to_string(&prompt_out).unwrap();
    assert!(p.contains("## Operator instructions"), "{p}");
    assert!(p.contains("focus on X"), "{p}");
}

#[test]
fn cancel_refuses_terminal_dream() {
    let f = common::fixture();
    let out = dream(&f)
        .arg("--force")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let id = dream_id(&String::from_utf8(out.stdout).unwrap());
    common::cmd(&f)
        .args(["cancel", &id])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused: dream is completed"));
}

#[test]
fn aggregator_argv_is_contained() {
    let f = common::fixture();
    let argv_out = f.root.path().join("argv.txt");
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_ARGV_OUT", &argv_out)
        .assert()
        .code(0);
    let all = std::fs::read_to_string(&argv_out).unwrap();
    // Prompts span lines and calls run concurrently, so scan flag/value pairs:
    // workers run in plan mode, the single aggregator in acceptEdits.
    let lines: Vec<&str> = all.lines().collect();
    let modes: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0] == "--permission-mode")
        .map(|w| w[1])
        .collect();
    assert_eq!(
        modes.iter().filter(|m| **m == "acceptEdits").count(),
        1,
        "{modes:?}"
    );
    assert_eq!(
        modes.iter().filter(|m| **m == "plan").count(),
        3,
        "{modes:?}"
    );
    assert_eq!(modes.len(), 4, "{modes:?}");
    let tools: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0] == "--allowedTools")
        .map(|w| w[1])
        .collect();
    assert_eq!(
        tools,
        vec!["Read,Glob,Grep,Write(./**),Edit(./**)"],
        "{all}"
    );
    assert!(!all.contains("bypassPermissions"), "{all}");
    assert!(!all.contains("Bash("), "{all}");
    assert!(!lines.contains(&"--add-dir"), "{all}");
}

#[test]
fn delete_manifest_removes_listed_file() {
    let f = common::fixture();
    let out = dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_DELETE", "1")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("+1 -1 ~1 files"), "{stdout}");
    let dir = only_dream_dir(&f);
    assert!(!dir.join("out/fact-b.md").exists(), "fact-b.md deleted");
    assert!(!dir.join("out/.dreamd-delete").exists(), "manifest removed");
    assert!(f.memory_dir.join("fact-b.md").is_file(), "live untouched");
    let diff = std::fs::read_to_string(dir.join("dream.diff")).unwrap();
    assert!(diff.contains("--- a/fact-b.md"), "{diff}");
    assert!(!diff.contains(".dreamd-delete"), "{diff}");
}

#[test]
fn bad_delete_entry_fails_the_dream() {
    let f = common::fixture();
    // Reuse the stub's escape hook to write a bogus manifest line
    // (`escaped write`) into out/.dreamd-delete.
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_ESCAPE", ".dreamd-delete")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("bad delete entry: escaped write"));
}

#[test]
fn live_memory_change_during_aggregation_fails_the_dream() {
    let f = common::fixture();
    let live = f.memory_dir.join("fact-a.md");
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_TOUCH_LIVE", &live)
        .assert()
        .code(4)
        .stderr(predicate::str::contains(
            "live memory dir changed during aggregation",
        ));
    let id = only_dream_dir(&f)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    common::cmd(&f)
        .args(["show", &id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("failed"))
        .stdout(predicate::str::contains(
            "live memory dir changed during aggregation",
        ));
}

/// Point the fixture project's transcripts (and so its discovered cwd) at `proj`.
fn retarget_cwd(f: &common::Fixture, proj: &Path) {
    for t in f.transcripts() {
        let text = std::fs::read_to_string(&t).unwrap();
        let text = text.replace(
            "\"/tmp/proj\"",
            &format!("{:?}", proj.display().to_string()),
        );
        std::fs::write(&t, text).unwrap();
        common::set_aged(&t);
    }
}

#[test]
fn project_claude_md_is_copied_to_context() {
    let f = common::fixture();
    let proj = f.root.path().join("proj");
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::write(proj.join(".claude/CLAUDE.md"), "house rules\n").unwrap();
    std::fs::write(proj.join("AGENTS.md"), "agents\n").unwrap();
    retarget_cwd(&f, &proj);
    let prompt_out = f.root.path().join("prompt.txt");
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_PROMPT_OUT", &prompt_out)
        .assert()
        .code(0);
    let dir = only_dream_dir(&f);
    assert_eq!(
        std::fs::read_to_string(dir.join("context/CLAUDE.md")).unwrap(),
        "house rules\n"
    );
    assert!(dir.join("context/AGENTS.md").is_file());
    let p = std::fs::read_to_string(&prompt_out).unwrap();
    assert!(p.contains("../context/CLAUDE.md"), "{p}");
    assert!(p.contains("../context/AGENTS.md"), "{p}");
    assert!(!p.contains(&proj.display().to_string()), "{p}");
}

#[test]
fn no_claude_md_prompt_skips_reconcile() {
    let f = common::fixture();
    let proj = f.root.path().join("bare-proj");
    std::fs::create_dir_all(&proj).unwrap();
    retarget_cwd(&f, &proj);
    let prompt_out = f.root.path().join("prompt.txt");
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_PROMPT_OUT", &prompt_out)
        .assert()
        .code(0);
    let p = std::fs::read_to_string(&prompt_out).unwrap();
    assert!(p.contains("no CLAUDE.md"), "{p}");
    assert!(!p.contains("../context/CLAUDE.md"), "{p}");
    assert!(!only_dream_dir(&f).join("context").exists());
}

fn failed_dream_show(f: &common::Fixture) -> String {
    let id = only_dream_dir(f)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let out = common::cmd(f)
        .args(["show", &id])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn aggregator_in_bypass_mode_fails_the_dream() {
    let f = common::fixture();
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_PERMISSION_MODE", "bypassPermissions")
        .env("DREAMD_STUB_PERMISSION_MODE_FOR", "aggregator")
        .assert()
        .code(4)
        .stderr(predicate::str::contains(
            "aggregator ran with permission mode bypassPermissions, expected acceptEdits (is claude_bin a wrapper that adds --dangerously-skip-permissions?)",
        ));
    let show = failed_dream_show(&f);
    assert!(show.contains("failed"), "{show}");
    assert!(
        show.contains("aggregator ran with permission mode"),
        "{show}"
    );
}

#[test]
fn workers_in_bypass_mode_are_failed() {
    let f = common::fixture();
    dream(&f)
        .arg("--force")
        .env("DREAMD_STUB_PERMISSION_MODE", "bypassPermissions")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("all workers failed"));
    let show = failed_dream_show(&f);
    assert_eq!(
        show.matches("worker ran with permission mode bypassPermissions, expected plan")
            .count(),
        3,
        "{show}"
    );
}

/// An executable script in the fixture root that execs the stub and
/// mentions the bypass flag, like the account-rotating wrapper does.
fn bypass_wrapper(f: &common::Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = f.root.path().join("claude-wrapper");
    let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stub-claude.sh");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n# would add --dangerously-skip-permissions\nexec {} \"$@\"\n",
            stub.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn bypass_wrapper_claude_bin_is_refused() {
    let f = common::fixture();
    let w = bypass_wrapper(&f);
    dream(&f)
        .arg("--force")
        .env("DREAMD_CLAUDE_BIN", &w)
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "claude_bin {} is a wrapper that injects --dangerously-skip-permissions; set claude_bin in ~/.config/dreamd/config.toml (or DREAMD_CLAUDE_BIN) to the real binary",
            w.display()
        )));
    assert!(!f.data_dir.join("dreams").exists(), "no dream created");
    // Explicit opt-out.
    dream(&f)
        .arg("--force")
        .env("DREAMD_CLAUDE_BIN", &w)
        .env("DREAMD_ALLOW_BYPASS_WRAPPER", "1")
        .assert()
        .code(0);
}

#[test]
fn doctor_flags_bypass_wrapper() {
    let f = common::fixture();
    let w = bypass_wrapper(&f);
    let out = common::cmd(&f)
        .env("DREAMD_CLAUDE_BIN", &w)
        .arg("doctor")
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let line = stdout
        .lines()
        .find(|l| l.contains("claude permissions"))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(line.starts_with("FAIL"), "{line}");
    assert!(line.contains("is a wrapper that injects"), "{line}");
    let ok = common::cmd(&f)
        .arg("doctor")
        .assert()
        .get_output()
        .stdout
        .clone();
    let ok = String::from_utf8(ok).unwrap();
    let line = ok
        .lines()
        .find(|l| l.contains("claude permissions"))
        .unwrap();
    assert!(line.starts_with("ok"), "{line}");
}
