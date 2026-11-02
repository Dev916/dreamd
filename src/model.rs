//! Data model shared by the pipeline, the store and the API.
// Most of these types are produced/consumed by later stages (pipeline, api).
#![allow(dead_code)]

use std::fmt;
use std::ops::AddAssign;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One Claude Code project in one config dir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub config_dir: PathBuf,
    pub slug: String,
    pub cwd: Option<PathBuf>,
    pub memory_dir: PathBuf,
}

impl Project {
    /// Stable identity across runs: `"{config_dir}:{slug}"`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.config_dir.display(), self.slug)
    }
}

/// A session transcript on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    pub mtime: DateTime<Utc>,
}

/// Lifecycle of a dream (mirrors the hosted Dreams API plus apply/rollback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
    Applied,
    RolledBack,
}

impl DreamStatus {
    pub const ALL: [DreamStatus; 7] = [
        DreamStatus::Pending,
        DreamStatus::Running,
        DreamStatus::Completed,
        DreamStatus::Failed,
        DreamStatus::Canceled,
        DreamStatus::Applied,
        DreamStatus::RolledBack,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DreamStatus::Pending => "pending",
            DreamStatus::Running => "running",
            DreamStatus::Completed => "completed",
            DreamStatus::Failed => "failed",
            DreamStatus::Canceled => "canceled",
            DreamStatus::Applied => "applied",
            DreamStatus::RolledBack => "rolled_back",
        }
    }

    /// The dream run is over (it may still be applied / rolled back later).
    pub fn is_terminal(self) -> bool {
        !matches!(self, DreamStatus::Pending | DreamStatus::Running)
    }
}

impl fmt::Display for DreamStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DreamStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        DreamStatus::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| anyhow::anyhow!("unknown dream status: {s}"))
    }
}

/// Token and cost accounting, summed across workers and the aggregator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Usage) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cost_usd += rhs.cost_usd;
    }
}

/// File-level counts of the in/ → out/ diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    pub files_added: usize,
    pub files_removed: usize,
    pub files_changed: usize,
}

/// A dream: one consolidation run over one project's memory dir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dream {
    pub id: String,
    pub project: Project,
    pub status: DreamStatus,
    pub model: String,
    pub instructions: Option<String>,
    pub session_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub usage: Usage,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub output_dir: PathBuf,
    pub diff_stats: Option<DiffStats>,
    pub pid: Option<u32>,
}

impl Dream {
    /// `"drm_" + ULID`.
    pub fn new_id() -> String {
        format!("drm_{}", ulid::Ulid::new())
    }
}

/// Per-session worker outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Ok,
    Failed,
    TimedOut,
}

impl SessionStatus {
    pub const ALL: [SessionStatus; 4] = [
        SessionStatus::Pending,
        SessionStatus::Ok,
        SessionStatus::Failed,
        SessionStatus::TimedOut,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Pending => "pending",
            SessionStatus::Ok => "ok",
            SessionStatus::Failed => "failed",
            SessionStatus::TimedOut => "timed_out",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        SessionStatus::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| anyhow::anyhow!("unknown session status: {s}"))
    }
}

/// One worker (session) inside a dream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub status: SessionStatus,
    pub findings_path: Option<PathBuf>,
    pub usage: Usage,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Worker output (validated by `findings_json_schema`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Findings {
    pub insights: Vec<Insight>,
    pub stale: Vec<Stale>,
    pub corrections: Vec<Correction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Insight {
    pub kind: MemoryKind,
    pub statement: String,
    pub why: String,
    pub evidence: Evidence,
    pub confidence: f32,
    pub relates_to: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    Reference,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub session_id: String,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stale {
    pub file: String,
    pub reason: String,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    pub file: String,
    pub wrong: String,
    pub right: String,
    pub evidence: Evidence,
}

fn strict_object(props: Value) -> Value {
    let required: Vec<Value> = props
        .as_object()
        .map(|m| m.keys().map(|k| Value::String(k.clone())).collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": props,
    })
}

/// Strict JSON schema for worker `Findings` (passed to `claude --json-schema`).
pub fn findings_json_schema() -> Value {
    let s = json!({"type": "string"});
    let evidence = strict_object(json!({"session_id": s, "quote": s}));
    let insight = strict_object(json!({
        "kind": {"type": "string", "enum": ["user", "feedback", "project", "reference"]},
        "statement": s,
        "why": s,
        "evidence": evidence,
        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
        "relates_to": {"type": "array", "items": s},
    }));
    let stale = strict_object(json!({"file": s, "reason": s, "evidence": evidence}));
    let correction = strict_object(json!({
        "file": s, "wrong": s, "right": s, "evidence": evidence,
    }));
    strict_object(json!({
        "insights": {"type": "array", "items": insight},
        "stale": {"type": "array", "items": stale},
        "corrections": {"type": "array", "items": correction},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display_fromstr_roundtrip() {
        for st in DreamStatus::ALL {
            assert_eq!(st.to_string().parse::<DreamStatus>().unwrap(), st);
            assert_eq!(
                serde_json::to_value(st).unwrap(),
                Value::String(st.to_string())
            );
        }
        assert_eq!(DreamStatus::RolledBack.to_string(), "rolled_back");
        assert!("nope".parse::<DreamStatus>().is_err());
        for st in SessionStatus::ALL {
            assert_eq!(st.to_string().parse::<SessionStatus>().unwrap(), st);
        }
    }

    #[test]
    fn ids_and_keys() {
        let id = Dream::new_id();
        assert!(id.starts_with("drm_") && id.len() == 30, "{id}");
        let p = Project {
            config_dir: "/c".into(),
            slug: "s".into(),
            cwd: None,
            memory_dir: "/c/m".into(),
        };
        assert_eq!(p.key(), "/c:s");
    }

    #[test]
    fn usage_add_assign() {
        let mut u = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cost_usd: 0.5,
        };
        u += Usage {
            input_tokens: 10,
            output_tokens: 20,
            cost_usd: 0.25,
        };
        assert_eq!(
            u,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                cost_usd: 0.75
            }
        );
    }

    #[test]
    fn schema_is_strict_and_matches_type() {
        let sc = findings_json_schema();
        assert_eq!(sc["additionalProperties"], false);
        let ins = &sc["properties"]["insights"]["items"];
        assert_eq!(ins["additionalProperties"], false);
        assert_eq!(ins["required"].as_array().unwrap().len(), 6);
        assert_eq!(ins["properties"]["confidence"]["maximum"], 1);
        let f: Findings = serde_json::from_value(json!({
            "insights": [{"kind": "feedback", "statement": "a", "why": "b",
                "evidence": {"session_id": "s1", "quote": "q"},
                "confidence": 0.9, "relates_to": ["x.md"]}],
            "stale": [], "corrections": []
        }))
        .unwrap();
        assert_eq!(f.insights[0].kind, MemoryKind::Feedback);
    }
}
