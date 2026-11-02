//! The `claude -p` runner. Every model call in dreamd goes through the
//! [`Claude`] trait so tests can swap in `tests/stub-claude.sh` (via
//! `DREAMD_CLAUDE_BIN`) or an in-process fake.

use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::model::Usage;

/// One `claude -p` invocation.
#[derive(Debug, Clone)]
pub struct ClaudeRequest {
    pub prompt: String,
    pub model: String,
    pub effort: String,
    pub json_schema: Option<Value>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
}

impl ClaudeRequest {
    /// A plain text request with no schema, tools or cwd.
    pub fn new(prompt: impl Into<String>, model: &str, effort: &str, timeout: Duration) -> Self {
        Self {
            prompt: prompt.into(),
            model: model.to_string(),
            effort: effort.to_string(),
            json_schema: None,
            permission_mode: None,
            allowed_tools: Vec::new(),
            add_dirs: Vec::new(),
            cwd: None,
            timeout,
        }
    }
}

/// The `type: "result"` event of a `claude -p --output-format json` run.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeResult {
    pub text: String,
    /// `structured_output` (set when `--json-schema` was passed), else the
    /// result text parsed as a JSON object/array when it is one.
    pub structured: Option<Value>,
    pub is_error: bool,
    pub usage: Usage,
    pub duration_ms: u64,
    pub session_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("failed to spawn claude: {0}")]
    Spawn(io::Error),
    #[error("claude exited with code {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("timed out after {}s", .0.as_secs())]
    Timeout(Duration),
    #[error("unparseable claude output: {0}")]
    BadOutput(String),
    #[error("model error: {0}")]
    ModelError(String),
}

#[async_trait]
pub trait Claude: Send + Sync {
    async fn run(&self, req: ClaudeRequest) -> Result<ClaudeResult, ClaudeError>;
}

/// Runs the real (or stub) `claude` binary by PATH name or path.
#[derive(Debug, Clone)]
pub struct CliClaude {
    pub bin: String,
}

impl CliClaude {
    /// argv after the binary name.
    pub fn args(req: &ClaudeRequest) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-p".into(),
            req.prompt.clone(),
            "--output-format".into(),
            "json".into(),
            "--no-session-persistence".into(),
            "--model".into(),
            req.model.clone(),
            "--effort".into(),
            req.effort.clone(),
        ];
        if let Some(s) = &req.json_schema {
            a.push("--json-schema".into());
            a.push(s.to_string());
        }
        if let Some(p) = &req.permission_mode {
            a.push("--permission-mode".into());
            a.push(p.clone());
        }
        if !req.allowed_tools.is_empty() {
            a.push("--allowedTools".into());
            a.push(req.allowed_tools.join(","));
        }
        for d in &req.add_dirs {
            a.push("--add-dir".into());
            a.push(d.display().to_string());
        }
        a
    }
}

/// Best-effort SIGKILL of the child's whole process group (claude spawns
/// helpers such as MCP servers; the stub spawns `sleep`).
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[async_trait]
impl Claude for CliClaude {
    async fn run(&self, req: ClaudeRequest) -> Result<ClaudeResult, ClaudeError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(Self::args(&req))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        let child = cmd.spawn().map_err(ClaudeError::Spawn)?;
        let pid = child.id();
        // On timeout the `wait_with_output` future (and with it the Child) is
        // dropped, which SIGKILLs the child via kill_on_drop; we also kill the
        // process group so grandchildren do not linger.
        let out = match tokio::time::timeout(req.timeout, child.wait_with_output()).await {
            Ok(r) => r.map_err(ClaudeError::Spawn)?,
            Err(_) => {
                kill_group(pid);
                return Err(ClaudeError::Timeout(req.timeout));
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        // A result event wins over the exit code: claude exits non-zero when
        // the result reports is_error, and the result text is the better message.
        match parse_output(&stdout) {
            Err(ClaudeError::BadOutput(_)) if !out.status.success() => {
                Err(ClaudeError::NonZeroExit {
                    code: out.status.code().unwrap_or(-1),
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                })
            }
            other => other,
        }
    }
}

fn excerpt(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Parse `claude -p --output-format json` stdout: a JSON array of events
/// whose last `type == "result"` element carries the outcome.
pub fn parse_output(stdout: &str) -> Result<ClaudeResult, ClaudeError> {
    let v: Value = serde_json::from_str(stdout)
        .map_err(|e| ClaudeError::BadOutput(format!("{e}: {}", excerpt(stdout))))?;
    let events = v.as_array().ok_or_else(|| {
        ClaudeError::BadOutput(format!("expected a JSON array: {}", excerpt(stdout)))
    })?;
    let r = events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("result"))
        .ok_or_else(|| ClaudeError::BadOutput("no result event".into()))?;
    let text = r
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let is_error = r.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    if is_error {
        let msg = if text.is_empty() {
            r.get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string()
        } else {
            text
        };
        return Err(ClaudeError::ModelError(msg));
    }
    let structured = match r.get("structured_output") {
        Some(s) if !s.is_null() => Some(s.clone()),
        _ => serde_json::from_str::<Value>(text.trim())
            .ok()
            .filter(|v| v.is_object() || v.is_array()),
    };
    let u = r.get("usage");
    let tok = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let usage = Usage {
        input_tokens: tok("input_tokens"),
        output_tokens: tok("output_tokens"),
        cost_usd: r
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    };
    Ok(ClaudeResult {
        text,
        structured,
        is_error,
        usage,
        duration_ms: r.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
        session_id: r
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn events(result: Value) -> String {
        json!([
            {"type": "system", "subtype": "init", "session_id": "s"},
            {"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}},
            result
        ])
        .to_string()
    }

    #[test]
    fn parse_output_reads_last_result_event() {
        let out = events(json!({
            "type": "result", "subtype": "success", "is_error": false,
            "result": "pong", "total_cost_usd": 0.0012,
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 99},
            "duration_ms": 1234, "session_id": "abc"
        }));
        let r = parse_output(&out).unwrap();
        assert_eq!(r.text, "pong");
        assert!(!r.is_error);
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 5);
        assert!((r.usage.cost_usd - 0.0012).abs() < 1e-12);
        assert_eq!(r.duration_ms, 1234);
        assert_eq!(r.session_id, "abc");
        assert_eq!(r.structured, None);
    }

    #[test]
    fn parse_output_structured_from_field_or_text() {
        // Field wins (shape confirmed against a real `claude -p --json-schema` run).
        let out = events(json!({
            "type": "result", "is_error": false, "result": "{\"a\":2}",
            "structured_output": {"a": 1}
        }));
        assert_eq!(
            parse_output(&out).unwrap().structured,
            Some(json!({"a": 1}))
        );
        // Fallback: result text that is a JSON object.
        let out = events(json!({"type": "result", "is_error": false, "result": " {\"a\":3} "}));
        assert_eq!(
            parse_output(&out).unwrap().structured,
            Some(json!({"a": 3}))
        );
        // Plain text: none.
        let out = events(json!({"type": "result", "is_error": false, "result": "hello"}));
        assert_eq!(parse_output(&out).unwrap().structured, None);
    }

    #[test]
    fn parse_output_is_error_maps_to_model_error() {
        let out = events(
            json!({"type": "result", "subtype": "success", "is_error": true, "result": "boom"}),
        );
        match parse_output(&out) {
            Err(ClaudeError::ModelError(m)) => assert_eq!(m, "boom"),
            other => panic!("expected ModelError, got {other:?}"),
        }
    }

    #[test]
    fn parse_output_rejects_non_array() {
        assert!(matches!(
            parse_output("not json"),
            Err(ClaudeError::BadOutput(_))
        ));
        let obj = json!({"type": "result", "is_error": false, "result": "x"}).to_string();
        assert!(matches!(parse_output(&obj), Err(ClaudeError::BadOutput(_))));
        assert!(matches!(
            parse_output(&json!([{"type": "system"}]).to_string()),
            Err(ClaudeError::BadOutput(_))
        ));
    }

    #[test]
    fn args_include_optional_flags_in_order() {
        let mut req = ClaudeRequest::new("hi", "sonnet", "low", Duration::from_secs(1));
        req.json_schema = Some(json!({"type": "object"}));
        req.permission_mode = Some("plan".into());
        req.allowed_tools = vec!["Read".into(), "Bash(ls:*)".into()];
        req.add_dirs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let a = CliClaude::args(&req);
        assert_eq!(
            &a[..5],
            [
                "-p",
                "hi",
                "--output-format",
                "json",
                "--no-session-persistence"
            ]
        );
        assert_eq!(&a[5..9], ["--model", "sonnet", "--effort", "low"]);
        assert_eq!(&a[9..11], ["--json-schema", "{\"type\":\"object\"}"]);
        assert_eq!(&a[11..13], ["--permission-mode", "plan"]);
        assert_eq!(&a[13..15], ["--allowedTools", "Read,Bash(ls:*)"]);
        assert_eq!(&a[15..], ["--add-dir", "/a", "--add-dir", "/b"]);
    }

    #[tokio::test]
    async fn timeout_kills_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = dir.path().join("slow.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 2\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let c = CliClaude {
            bin: script.display().to_string(),
        };
        let req = ClaudeRequest::new("x", "m", "e", Duration::from_millis(300));
        assert!(matches!(c.run(req).await, Err(ClaudeError::Timeout(_))));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "child kept running after timeout");
    }
}
