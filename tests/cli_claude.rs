//! `dreamd claude-probe` (hidden debug command): Task 5 acceptance criteria,
//! run against `tests/stub-claude.sh`.

mod common;

use std::time::{Duration, Instant};

use predicates::prelude::*;

#[test]
fn probe_echo_prints_text_and_usage() {
    let f = common::fixture();
    common::cmd(&f)
        .env_remove("DREAMD_STUB_MODE")
        .args(["claude-probe", "reply with exactly: pong"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("pong"))
        .stdout(predicate::str::contains("usage:"))
        .stdout(predicate::str::contains("cost_usd"));
}

#[test]
fn probe_fail_mode_exits_4_with_claude_error() {
    let f = common::fixture();
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "fail")
        .args(["claude-probe", "anything"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("claude error:"))
        .stderr(predicate::str::contains("boom"));
}

#[test]
fn probe_badjson_mode_exits_4() {
    let f = common::fixture();
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "badjson")
        .args(["claude-probe", "anything"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("claude error:"));
}

#[test]
fn probe_sleep_mode_times_out_and_kills_child() {
    let f = common::fixture();
    let start = Instant::now();
    common::cmd(&f)
        .env("DREAMD_STUB_MODE", "sleep")
        .args(["claude-probe", "anything", "--timeout", "1"])
        .timeout(Duration::from_secs(20))
        .assert()
        .code(4)
        .stderr(predicate::str::contains("timed out after 1s"));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "probe took {:?}",
        start.elapsed()
    );
}
