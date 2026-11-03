//! Per-session workers: one `claude -p` per transcript digest, fanned out
//! under a semaphore. A failed or timed-out worker is recorded on its own
//! session and never aborts the others.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::claude::{Claude, ClaudeError, ClaudeRequest};
use crate::config::WorkerCfg;
use crate::model::{findings_json_schema, Findings, SessionRecord, SessionStatus, Usage};
use crate::transcript::Digest;

/// Grace on top of the request timeout before the fan-out gives up on a
/// worker itself (the runner normally times out and kills claude first).
const TIMEOUT_GRACE: Duration = Duration::from_secs(2);

/// Workers need no tools: the digest is inlined in the prompt.
pub const WORKER_PERMISSION_MODE: &str = "plan";

/// Prompt paragraph for a delta digest (a session mined before that grew).
pub const DELTA_NOTE: &str = "This session was mined before and has grown since. The digest holds only the new part, under `# new since the last dream`; the lines under `# earlier context` were already consolidated and are there for continuity only. Mine only the new part: never report an insight, stale entry or correction whose evidence is in the earlier context.\n\n";

/// The worker prompt for one session digest.
pub fn worker_prompt(memory_index: &str, digest: &Digest) -> String {
    let index = if memory_index.trim().is_empty() {
        "(empty: this project has no memories yet)"
    } else {
        memory_index.trim_end()
    };
    format!(
        "You are mining one Claude Code session transcript for durable memories about this project.\n\
\n\
Memory kinds (use exactly one per insight):\n\
- user: who the user is, their role, preferences and expertise.\n\
- feedback: guidance the user gave on how to work (corrections, \"always\"/\"never\" rules, confirmed approaches).\n\
- project: facts about the project itself (layout, conventions, decisions, ongoing work).\n\
- reference: pointers to external resources (URLs, docs, dashboards, tickets).\n\
\n\
{scope}Rules:\n\
- Every insight, stale entry and correction MUST cite evidence: this session id and a verbatim quote from the transcript below. No quote, no finding.\n\
- Insights only; do not restate the memory index. Report only what this session adds, contradicts (corrections) or shows to be out of date (stale).\n\
- Skip ephemeral task chatter that will not matter in a future session.\n\
- Use `relates_to` for memory file names from the index that an insight touches.\n\
- Return empty arrays when the session holds nothing durable.\n\
\n\
session id: {sid}\n\
\n\
## Existing memory index (MEMORY.md)\n\
{index}\n\
\n\
## Transcript digest\n\
{digest}\n",
        sid = digest.session_id,
        digest = digest.text,
        scope = if digest.delta { DELTA_NOTE } else { "" },
    )
}

/// One session's result: its record plus the parsed findings on success.
#[derive(Debug, Clone)]
pub struct SessionOutcome {
    pub record: SessionRecord,
    #[allow(dead_code)] // parsed findings, for in-process callers (API, tests)
    pub findings: Option<Findings>,
}

/// Run one worker per digest (at most `cfg.concurrency` at a time) and write
/// `findings_dir/<session_id>.json` for each success. Outcomes come back in
/// input order. Never panics on a worker error. `Digest` carries no path, so
/// each record's `transcript_path` is empty; the caller fills it in.
pub async fn mine_sessions(
    claude: Arc<dyn Claude>,
    cfg: &WorkerCfg,
    memory_index: String,
    digests: Vec<Digest>,
    findings_dir: &Path,
) -> Vec<SessionOutcome> {
    let sem = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let schema = findings_json_schema();
    let dir_err = std::fs::create_dir_all(findings_dir)
        .err()
        .map(|e| format!("create {}: {e}", findings_dir.display()));
    let mut set = JoinSet::new();
    let mut sids: Vec<String> = Vec::with_capacity(digests.len());
    for (i, digest) in digests.into_iter().enumerate() {
        sids.push(digest.session_id.clone());
        let mut req = ClaudeRequest::new(
            worker_prompt(&memory_index, &digest),
            &cfg.model,
            &cfg.effort,
            timeout,
        );
        req.permission_mode = Some(WORKER_PERMISSION_MODE.into());
        req.json_schema = Some(schema.clone());
        let claude = Arc::clone(&claude);
        let sem = Arc::clone(&sem);
        let findings_dir = findings_dir.to_path_buf();
        let dir_err = dir_err.clone();
        set.spawn(async move {
            // The semaphore is never closed, so acquire only fails if it
            // were; treat that as "run anyway".
            let _permit = sem.acquire_owned().await.ok();
            let start = Instant::now();
            let res = match tokio::time::timeout(timeout + TIMEOUT_GRACE, claude.run(req)).await {
                Ok(r) => r,
                Err(_) => Err(ClaudeError::Timeout(timeout)),
            };
            let elapsed = start.elapsed().as_millis() as u64;
            (
                i,
                finish(&digest.session_id, res, elapsed, &findings_dir, dir_err),
            )
        });
    }

    let mut slots: Vec<Option<SessionOutcome>> = vec![None; sids.len()];
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((i, outcome)) => slots[i] = Some(outcome),
            // A panicking worker task: its slot stays None and is filled below.
            Err(e) => tracing::warn!("worker task failed: {e}"),
        }
    }
    slots
        .into_iter()
        .zip(sids)
        .map(|(slot, sid)| slot.unwrap_or_else(|| failed(&sid, "worker task panicked".into(), 0)))
        .collect()
}

fn failed(sid: &str, error: String, duration_ms: u64) -> SessionOutcome {
    SessionOutcome {
        record: SessionRecord {
            session_id: sid.to_string(),
            transcript_path: PathBuf::new(),
            status: SessionStatus::Failed,
            findings_path: None,
            usage: Usage::default(),
            error: Some(error),
            duration_ms,
            bytes_read: 0,
        },
        findings: None,
    }
}

/// Turn one runner result into an outcome, writing the findings file on success.
fn finish(
    sid: &str,
    res: Result<crate::claude::ClaudeResult, ClaudeError>,
    duration_ms: u64,
    findings_dir: &Path,
    dir_err: Option<String>,
) -> SessionOutcome {
    let r = match res {
        Ok(r) => r,
        Err(e @ ClaudeError::Timeout(_)) => {
            let mut o = failed(sid, e.to_string(), duration_ms);
            o.record.status = SessionStatus::TimedOut;
            return o;
        }
        Err(e) => return failed(sid, e.to_string(), duration_ms),
    };
    let fail_with_usage = |msg: String| {
        let mut o = failed(sid, msg, duration_ms);
        o.record.usage = r.usage;
        o
    };
    if let Some(mode) = r
        .permission_mode
        .as_deref()
        .filter(|m| *m != WORKER_PERMISSION_MODE)
    {
        return fail_with_usage(format!(
            "worker ran with permission mode {mode}, expected {WORKER_PERMISSION_MODE} (is claude_bin a wrapper that adds --dangerously-skip-permissions?)"
        ));
    }
    let Some(value) = r.structured.clone() else {
        return fail_with_usage("worker returned no structured output".into());
    };
    let findings: Findings = match serde_json::from_value(value) {
        Ok(f) => f,
        Err(e) => return fail_with_usage(format!("invalid findings: {e}")),
    };
    if let Some(e) = dir_err {
        return fail_with_usage(e);
    }
    let path = findings_dir.join(format!("{sid}.json"));
    let write = serde_json::to_string_pretty(&findings)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(&path, s + "\n").map_err(|e| e.to_string()));
    if let Err(e) = write {
        return fail_with_usage(format!("write {}: {e}", path.display()));
    }
    SessionOutcome {
        record: SessionRecord {
            session_id: sid.to_string(),
            transcript_path: PathBuf::new(),
            status: SessionStatus::Ok,
            findings_path: Some(path),
            usage: r.usage,
            error: None,
            duration_ms,
            bytes_read: 0,
        },
        findings: Some(findings),
    }
}

/// True when there was at least one session and none succeeded.
pub fn all_failed(outcomes: &[SessionOutcome]) -> bool {
    !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|o| o.record.status != SessionStatus::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeResult;
    use async_trait::async_trait;
    use serde_json::json;

    fn digest(id: &str) -> Digest {
        crate::transcript::reduce_str(
            id,
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            10_000,
        )
    }

    #[test]
    fn worker_prompt_embeds_session_id_and_index() {
        let d = digest("abc-123");
        let p = worker_prompt("- [Fact A](fact-a.md) — scripts live under tools/", &d);
        assert!(p.lines().any(|l| l == "session id: abc-123"), "{p}");
        assert!(p.contains("[Fact A](fact-a.md)"));
        for kind in ["- user:", "- feedback:", "- project:", "- reference:"] {
            assert!(p.contains(kind), "missing {kind}");
        }
        assert!(
            p.contains("insights only; do not restate the memory index")
                || p.contains("Insights only; do not restate the memory index")
        );
        assert!(p.contains("verbatim quote"));
        // session id line comes before the digest, digest is embedded whole.
        let sid_at = p.find("session id: abc-123").unwrap();
        let digest_at = p.find("# session abc-123").unwrap();
        assert!(sid_at < digest_at);
        assert!(p.contains(&d.text));
        assert!(worker_prompt("", &d).contains("no memories yet"));
        assert!(!p.contains("Mine only the new part"));
        let mut delta = d.clone();
        delta.delta = true;
        assert!(worker_prompt("", &delta).contains(DELTA_NOTE));
    }

    /// Fake runner keyed on the session id in the prompt.
    struct Fake;

    #[async_trait]
    impl Claude for Fake {
        async fn run(&self, req: ClaudeRequest) -> Result<ClaudeResult, ClaudeError> {
            assert_eq!(req.permission_mode.as_deref(), Some("plan"));
            assert!(req.json_schema.is_some());
            let sid = req
                .prompt
                .lines()
                .find_map(|l| l.strip_prefix("session id: "))
                .unwrap()
                .to_string();
            let ok = |structured| {
                Ok(ClaudeResult {
                    text: String::new(),
                    structured,
                    is_error: false,
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                        cost_usd: 0.5,
                    },
                    duration_ms: 1,
                    session_id: "x".into(),
                    permission_mode: None,
                })
            };
            match sid.as_str() {
                "timeout" => Err(ClaudeError::Timeout(req.timeout)),
                "error" => Err(ClaudeError::ModelError("boom".into())),
                "badjson" => Err(ClaudeError::BadOutput("not json".into())),
                "nonzero" => Err(ClaudeError::NonZeroExit {
                    code: 1,
                    stderr: "x".into(),
                }),
                "noschema" => ok(None),
                "wrongshape" => ok(Some(json!({"nope": 1}))),
                "panic" => panic!("worker exploded"),
                "hang" => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    unreachable!()
                }
                _ => ok(Some(json!({
                    "insights": [{"kind": "project", "statement": "s", "why": "w",
                        "evidence": {"session_id": sid, "quote": "q"},
                        "confidence": 0.5, "relates_to": []}],
                    "stale": [], "corrections": []
                }))),
            }
        }
    }

    fn cfg() -> WorkerCfg {
        WorkerCfg {
            concurrency: 2,
            timeout_secs: 1,
            ..WorkerCfg::default()
        }
    }

    #[tokio::test]
    async fn errors_map_to_statuses_in_input_order_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let ids = [
            "good",
            "timeout",
            "error",
            "badjson",
            "nonzero",
            "noschema",
            "wrongshape",
            "panic",
            "hang",
        ];
        let digests = ids.iter().map(|i| digest(i)).collect();
        let out = mine_sessions(Arc::new(Fake), &cfg(), String::new(), digests, dir.path()).await;
        let got: Vec<(&str, SessionStatus)> = out
            .iter()
            .map(|o| (o.record.session_id.as_str(), o.record.status))
            .collect();
        use SessionStatus::*;
        assert_eq!(
            got,
            vec![
                ("good", Ok),
                ("timeout", TimedOut),
                ("error", Failed),
                ("badjson", Failed),
                ("nonzero", Failed),
                ("noschema", Failed),
                ("wrongshape", Failed),
                ("panic", Failed),
                ("hang", TimedOut),
            ]
        );
        assert_eq!(out[1].record.error.as_deref(), Some("timed out after 1s"));
        assert_eq!(out[2].record.error.as_deref(), Some("model error: boom"));
        let good = &out[0];
        let path = good.record.findings_path.clone().unwrap();
        assert_eq!(path, dir.path().join("good.json"));
        let f: Findings = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(f.insights[0].evidence.session_id, "good");
        assert_eq!(good.findings.as_ref(), Some(&f));
        assert_eq!(good.record.usage.output_tokens, 2);
        assert!(!dir.path().join("timeout.json").exists());
        assert!(!all_failed(&out));
        assert!(all_failed(&out[1..]));
        assert!(!all_failed(&[]));
    }
}
