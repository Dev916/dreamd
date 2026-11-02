//! Transcript reducer: a Claude Code session JSONL → a compact text digest.

use std::path::Path;

/// A reduced transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub session_id: String,
    pub text: String,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub elided: bool,
    pub turns: usize,
    pub skipped_lines: usize,
}

use serde_json::Value;

/// Max chars of a `tool_use` input preview.
const TOOL_INPUT_PREVIEW: usize = 200;
/// Max chars of an error `tool_result`.
const TOOL_ERROR_PREVIEW: usize = 500;
/// A run of base64 alphabet longer than this is treated as binary.
const BASE64_RUN: usize = 100;
const BINARY: &str = "[binary omitted]";

/// Reduce the transcript at `path`; the session id is the file stem.
pub fn reduce(path: &Path, max_bytes: usize) -> anyhow::Result<Digest> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read transcript {}: {e}", path.display()))?;
    let jsonl = String::from_utf8_lossy(&bytes);
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(reduce_str(&session_id, &jsonl, max_bytes))
}

/// Reduce JSONL text. Never fails: malformed lines are counted in
/// `skipped_lines`, unknown record types are dropped.
pub fn reduce_str(session_id: &str, jsonl: &str, max_bytes: usize) -> Digest {
    let mut text = format!("# session {session_id}\n");
    let mut turns = 0;
    let mut skipped_lines = 0;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                skipped_lines += 1;
                continue;
            }
        };
        let kind = rec.get("type").and_then(Value::as_str).unwrap_or("");
        let role = match kind {
            "user" => "USER",
            "assistant" => "ASSISTANT",
            _ => continue,
        };
        let Some(content) = rec.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let lines = record_lines(role, content);
        if !lines.is_empty() {
            turns += 1;
            for l in lines {
                text.push_str(&l);
                text.push('\n');
            }
        }
    }
    let (text, elided) = truncate(text, max_bytes);
    Digest {
        session_id: session_id.to_string(),
        bytes_in: jsonl.len(),
        bytes_out: text.len(),
        text,
        elided,
        turns,
        skipped_lines,
    }
}

/// Digest lines for one user/assistant `message.content`.
fn record_lines(role: &str, content: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let push_text = |out: &mut Vec<String>, t: &str| {
        let t = sanitize(t.trim());
        if !t.is_empty() {
            out.push(format!("{role}: {t}"));
        }
    };
    match content {
        Value::String(s) => push_text(&mut out, s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            push_text(&mut out, t);
                        }
                    }
                    "tool_use" => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = b.get("input").map(tool_input_preview).unwrap_or_default();
                        out.push(format!("TOOL {name}: {input}"));
                    }
                    "tool_result" => {
                        if b.get("is_error").and_then(Value::as_bool) == Some(true) {
                            let body = b.get("content").map(result_text).unwrap_or_default();
                            let body = head_chars(&collapse(&sanitize(&body)), TOOL_ERROR_PREVIEW);
                            out.push(format!("TOOL_ERROR: {body}"));
                        }
                    }
                    // `image`, `thinking`, `document`, unknown: dropped.
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// Preview of a `tool_use` input: the value itself when the input is a
/// single string field (or has a `command` string, as Bash does), else the
/// compact JSON. Newlines collapsed, first 200 chars.
fn tool_input_preview(input: &Value) -> String {
    let raw = match input {
        Value::Object(m) => match m.get("command").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None if m.len() == 1 => match m.values().next() {
                Some(Value::String(s)) => s.clone(),
                _ => input.to_string(),
            },
            None => input.to_string(),
        },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    head_chars(&collapse(&sanitize(&raw)), TOOL_INPUT_PREVIEW)
}

/// Text of a `tool_result.content` (string or array of text blocks).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Collapse all whitespace runs (incl. newlines) to single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn head_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn is_b64(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

/// Replace base64-looking runs (> 100 chars of `[A-Za-z0-9+/=]`) with
/// `[binary omitted]`, swallowing a preceding `data:<mime>;base64,` prefix.
fn sanitize(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len().min(4096));
    let mut copied = 0; // bytes of `s` already handled
    let mut i = 0;
    while i < b.len() {
        if !is_b64(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_b64(b[i]) {
            i += 1;
        }
        if i - start > BASE64_RUN {
            // All bytes in the run are ASCII, so `start`/`i` are char boundaries.
            let cut = data_url_start(s, start).unwrap_or(start);
            out.push_str(&s[copied..cut]);
            out.push_str(BINARY);
            copied = i;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// If `s[..run_start]` ends with `data:<non-space>;base64,`, the index of `data:`.
fn data_url_start(s: &str, run_start: usize) -> Option<usize> {
    let before = &s[..run_start];
    if !before.ends_with(";base64,") {
        return None;
    }
    let at = before.rfind("data:")?;
    let mid = &before[at..];
    (!mid.contains(char::is_whitespace)).then_some(at)
}

/// Keep the first 40% and last 40% of `text` (by bytes, on line boundaries)
/// with an elision marker between, when `text` exceeds `max_bytes`.
fn truncate(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let budget = max_bytes * 2 / 5;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut head_len = 0;
    let mut head_n = 0;
    for l in &lines {
        if head_len + l.len() > budget {
            break;
        }
        head_len += l.len();
        head_n += 1;
    }
    let mut tail_len = 0;
    for l in lines[head_n..].iter().rev() {
        if tail_len + l.len() > budget {
            break;
        }
        tail_len += l.len();
    }
    let elided = text.len() - head_len - tail_len;
    let mut out = String::with_capacity(head_len + tail_len + 40);
    out.push_str(&text[..head_len]);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("[... elided {elided} bytes ...]\n"));
    out.push_str(&text[text.len() - tail_len..]);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: &str = include_str!("../tests/fixtures/transcripts/s1.jsonl");

    #[test]
    fn keeps_user_assistant_tool_lines() {
        let d = reduce_str("s1", S1, 200_000);
        assert!(d.text.starts_with("# session s1\n"), "{}", d.text);
        assert!(d
            .text
            .contains("USER: Add a --verbose flag to the build script"));
        assert!(d.text.contains("ASSISTANT: Let me look."));
        assert!(d.text.contains("TOOL Bash: ls scripts"));
        assert!(d
            .text
            .contains("TOOL_ERROR: ls: scripts: No such file or directory"));
        assert!(d.text.contains("ASSISTANT: Noted."));
        assert_eq!(d.skipped_lines, 0);
        assert!(!d.elided);
        assert_eq!(d.bytes_in, S1.len());
        assert_eq!(d.bytes_out, d.text.len());
        assert!(d.turns >= 6, "turns = {}", d.turns);
    }

    #[test]
    fn drops_progress_and_images() {
        let d = reduce_str("s1", S1, 200_000);
        assert!(!d.text.contains("running..."));
        assert!(!d.text.contains("session start"));
        let img = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"see pic"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo"}}]}}"#;
        let d = reduce_str("x", img, 200_000);
        assert!(d.text.contains("USER: see pic"));
        assert!(!d.text.contains("iVBORw0KGgo"));
        assert!(!d.text.contains("base64"));
    }

    #[test]
    fn base64_run_replaced() {
        let blob = "QUJD".repeat(500);
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"here: data:image/png;base64,{blob} done"}}]}}}}"#
        );
        let d = reduce_str("x", &line, 200_000);
        assert!(d.text.contains("[binary omitted]"), "{}", d.text);
        assert!(!d.text.contains("data:image"));
        assert!(!d.text.contains(&blob[..101]));
        assert!(d.text.contains("done"));
        // A short token survives.
        let line = r#"{"type":"user","message":{"role":"user","content":"sha abc123DEF456 ok"}}"#;
        assert!(reduce_str("x", line, 200_000).text.contains("abc123DEF456"));
    }

    #[test]
    fn malformed_line_counted_not_fatal() {
        let input = format!("{{not json\n\n{S1}");
        let d = reduce_str("s1", &input, 200_000);
        assert_eq!(d.skipped_lines, 1);
        assert!(d.text.contains("ASSISTANT: Noted."));
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let mut input = String::new();
        for i in 0..400 {
            input.push_str(&format!(
                r#"{{"type":"user","message":{{"role":"user","content":"message number {i:04} with some padding text"}}}}"#
            ));
            input.push('\n');
        }
        let d = reduce_str("big", &input, 2000);
        assert!(d.elided);
        assert!(d.text.len() <= 2000, "len {}", d.text.len());
        assert!(d.text.starts_with("# session big\n"));
        assert!(d.text.contains("message number 0000"));
        assert!(d.text.contains("message number 0399"));
        assert!(!d.text.contains("message number 0200"));
        assert!(d.text.contains("\n[... elided "));
        assert!(d.text.contains(" bytes ...]\n"));
        // Every kept line is whole.
        for l in d.text.lines() {
            assert!(
                l.starts_with('#')
                    || l.starts_with("USER: message number")
                    || l.starts_with("[... elided"),
                "partial line: {l:?}"
            );
        }
    }
}
