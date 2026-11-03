//! Transcript reducer: a Claude Code session JSONL → a compact text digest.

use std::path::Path;

/// A reduced transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub session_id: String,
    pub text: String,
    /// Transcript bytes read (only the new part for a delta read).
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub elided: bool,
    pub turns: usize,
    pub skipped_lines: usize,
    /// Absolute byte offset this read consumed through (the next mark).
    pub consumed_bytes: u64,
    /// Absolute JSONL record count this read consumed through (the next mark).
    pub consumed_records: usize,
    /// Only the part after an earlier mark was read.
    pub delta: bool,
}

/// Records of already-mined context prepended to a delta digest.
pub const OVERLAP_RECORDS: usize = 6;
/// Header of the already-mined context block in a delta digest.
pub const EARLIER_CONTEXT: &str = "# earlier context (already consolidated — do not re-report)";
/// Header of the new part in a delta digest.
pub const NEW_PART: &str = "# new since the last dream";
/// Max chars of one earlier-context line.
const OVERLAP_LINE_PREVIEW: usize = 500;

use serde_json::Value;

/// Max chars of a `tool_use` input preview.
const TOOL_INPUT_PREVIEW: usize = 200;
/// Max chars of an error `tool_result`.
const TOOL_ERROR_PREVIEW: usize = 500;
/// A run of base64 alphabet longer than this is treated as binary.
const BASE64_RUN: usize = 100;
const BINARY: &str = "[binary omitted]";

/// Reduce the whole transcript at `path`; the session id is the file stem.
pub fn reduce(path: &Path, max_bytes: usize) -> anyhow::Result<Digest> {
    reduce_from(path, 0, 0, 0, max_bytes)
}

/// Reduce the transcript at `path` from an earlier mark: skip the first
/// `start_records` JSONL records (which must end exactly at byte
/// `start_bytes`), and prepend up to `overlap_records` of the preceding
/// user/assistant text records under [`EARLIER_CONTEXT`]. When the mark no
/// longer fits the file (truncated or rewritten), warn and read it whole.
pub fn reduce_from(
    path: &Path,
    start_bytes: u64,
    start_records: usize,
    overlap_records: usize,
    max_bytes: usize,
) -> anyhow::Result<Digest> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read transcript {}: {e}", path.display()))?;
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let d = reduce_bytes_from(
        &session_id,
        &bytes,
        start_bytes,
        start_records,
        overlap_records,
        max_bytes,
    );
    if (start_bytes > 0 || start_records > 0) && !d.delta {
        tracing::warn!(
            "session {session_id}: mark ({start_bytes} bytes, {start_records} records) does not fit {} ({} bytes); re-reading it from the start",
            path.display(),
            bytes.len()
        );
    }
    Ok(d)
}

/// One JSONL line: `start..end` is its content (without the newline),
/// `next` the offset after it.
struct Span {
    start: usize,
    end: usize,
    next: usize,
}

/// Split `bytes` into lines; a trailing unterminated line is kept only when
/// it parses as JSON (a writer may still be mid-line).
fn spans(bytes: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        match bytes[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => {
                out.push(Span {
                    start: pos,
                    end: pos + i,
                    next: pos + i + 1,
                });
                pos += i + 1;
            }
            None => {
                let tail = &bytes[pos..];
                if serde_json::from_slice::<Value>(tail).is_ok() {
                    out.push(Span {
                        start: pos,
                        end: bytes.len(),
                        next: bytes.len(),
                    });
                }
                break;
            }
        }
    }
    out
}

/// [`reduce_from`] over in-memory bytes (no warning on fallback; check
/// `delta`).
pub fn reduce_bytes_from(
    session_id: &str,
    bytes: &[u8],
    start_bytes: u64,
    start_records: usize,
    overlap_records: usize,
    max_bytes: usize,
) -> Digest {
    let spans = spans(bytes);
    // The mark fits when its record count lands on a line boundary at
    // exactly `start_bytes` (before or after that line's newline).
    let fits = start_records > 0 && start_records <= spans.len() && {
        let last = &spans[start_records - 1];
        start_bytes == last.next as u64 || start_bytes == last.end as u64
    };
    let skip = if fits { start_records } else { 0 };
    let line_str = |sp: &Span| String::from_utf8_lossy(&bytes[sp.start..sp.end]).into_owned();

    let mut text = format!("# session {session_id}\n");
    if fits {
        let mut earlier: Vec<Vec<String>> = Vec::new();
        for sp in spans[..skip].iter().rev() {
            if earlier.len() >= overlap_records {
                break;
            }
            let Some((role, content)) = parse_turn(&line_str(sp)) else {
                continue;
            };
            let lines: Vec<String> = record_lines(role, &content)
                .into_iter()
                .filter(|l| l.starts_with("USER: ") || l.starts_with("ASSISTANT: "))
                .map(|l| head_chars(&l, OVERLAP_LINE_PREVIEW))
                .collect();
            if !lines.is_empty() {
                earlier.push(lines);
            }
        }
        if !earlier.is_empty() {
            text.push_str(EARLIER_CONTEXT);
            text.push('\n');
            for l in earlier.into_iter().rev().flatten() {
                text.push_str(&l);
                text.push('\n');
            }
        }
        text.push_str(NEW_PART);
        text.push('\n');
    }
    let new = &spans[skip..];
    let (body, turns, skipped_lines) = digest_lines(new.iter().map(line_str));
    text.push_str(&body);
    let (text, elided) = truncate(text, max_bytes);
    let read_from = if fits { spans[skip - 1].next } else { 0 };
    Digest {
        session_id: session_id.to_string(),
        bytes_in: bytes.len().saturating_sub(read_from),
        bytes_out: text.len(),
        text,
        elided,
        turns,
        skipped_lines,
        consumed_bytes: spans.last().map(|s| s.next).unwrap_or(0) as u64,
        consumed_records: spans.len(),
        delta: fits,
    }
}

/// Reduce JSONL text. Never fails: malformed lines are counted in
/// `skipped_lines`, unknown record types are dropped.
pub fn reduce_str(session_id: &str, jsonl: &str, max_bytes: usize) -> Digest {
    let (body, turns, skipped_lines) = digest_lines(jsonl.lines());
    let text = format!("# session {session_id}\n{body}");
    let (text, elided) = truncate(text, max_bytes);
    Digest {
        session_id: session_id.to_string(),
        bytes_in: jsonl.len(),
        bytes_out: text.len(),
        text,
        elided,
        turns,
        skipped_lines,
        consumed_bytes: jsonl.len() as u64,
        consumed_records: jsonl.lines().count(),
        delta: false,
    }
}

/// A user/assistant record's role label and `message.content`.
fn parse_turn(line: &str) -> Option<(&'static str, Value)> {
    turn_of(serde_json::from_str(line.trim()).ok()?)
}

fn turn_of(mut rec: Value) -> Option<(&'static str, Value)> {
    let role = match rec.get("type").and_then(Value::as_str).unwrap_or("") {
        "user" => "USER",
        "assistant" => "ASSISTANT",
        _ => return None,
    };
    let content = rec.get_mut("message")?.get_mut("content")?.take();
    Some((role, content))
}

/// Digest body (one line per kept item), turn count and malformed-line count.
fn digest_lines<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> (String, usize, usize) {
    let mut text = String::new();
    let mut turns = 0;
    let mut skipped_lines = 0;
    for line in lines {
        let line = line.as_ref().trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            skipped_lines += 1;
            continue;
        };
        let Some((role, content)) = turn_of(rec) else {
            continue;
        };
        let lines = record_lines(role, &content);
        if !lines.is_empty() {
            turns += 1;
            for l in lines {
                text.push_str(&l);
                text.push('\n');
            }
        }
    }
    (text, turns, skipped_lines)
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
            // Only swallow a data-URL prefix that lies in the not-yet-copied
            // region; an earlier `data:` (before `copied`) would make the slice
            // below panic on adjacent runs joined by `;base64,`.
            let cut = data_url_start(s, start)
                .filter(|&c| c >= copied)
                .unwrap_or(start);
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
    fn adjacent_base64_runs_with_base64_marker_do_not_panic() {
        // Regression: two long runs joined by ";base64," used to make
        // data_url_start() point before `copied`, panicking on the slice.
        let blob = "QUJD".repeat(500);
        let text = format!("data:image/png;base64,{blob};base64,{blob} tail");
        let out = sanitize(&text);
        assert_eq!(out.matches("[binary omitted]").count(), 2, "{out}");
        assert!(out.ends_with(" tail"));
        assert!(!out.contains(&blob[..101]));
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

    fn turn(role: &str, i: usize) -> String {
        format!(
            r#"{{"type":"{role}","message":{{"role":"{role}","content":"{role} message {i}"}}}}"#
        ) + "\n"
    }

    /// 10 alternating user/assistant records plus a progress record.
    fn ten() -> String {
        let mut s = String::new();
        for i in 0..10 {
            s.push_str(&turn(if i % 2 == 0 { "user" } else { "assistant" }, i));
        }
        s.push_str("{\"type\":\"progress\",\"data\":{}}\n");
        s
    }

    #[test]
    fn reduce_from_zero_consumes_everything() {
        let t = ten();
        let d = reduce_bytes_from("x", t.as_bytes(), 0, 0, 0, 200_000);
        assert!(!d.delta);
        assert_eq!(d.consumed_bytes, t.len() as u64);
        assert_eq!(d.consumed_records, 11);
        assert_eq!(d.bytes_in, t.len());
        assert_eq!(d.turns, 10);
        assert!(!d.text.contains(EARLIER_CONTEXT));
        // Same text as reduce_str.
        assert_eq!(d.text, reduce_str("x", &t, 200_000).text);
    }

    #[test]
    fn reduce_from_mark_reads_only_new_part_with_overlap() {
        let first = ten();
        let mut grown = first.clone();
        for i in 10..14 {
            grown.push_str(&turn(if i % 2 == 0 { "user" } else { "assistant" }, i));
        }
        let d = reduce_bytes_from("x", grown.as_bytes(), first.len() as u64, 11, 3, 200_000);
        assert!(d.delta, "{}", d.text);
        assert_eq!(d.consumed_bytes, grown.len() as u64);
        assert_eq!(d.consumed_records, 15);
        assert_eq!(d.bytes_in, grown.len() - first.len());
        assert_eq!(d.turns, 4);
        let text = &d.text;
        let earlier = text.find(EARLIER_CONTEXT).expect("earlier header");
        let new = text.find(NEW_PART).expect("new header");
        assert!(earlier < new);
        // The 3 records before the mark, in order, then the new ones.
        let ctx = &text[earlier..new];
        assert!(!ctx.contains("message 6"), "{ctx}");
        for i in 7..10 {
            assert!(ctx.contains(&format!("message {i}")), "{ctx}");
        }
        assert!(ctx.find("message 7").unwrap() < ctx.find("message 9").unwrap());
        let body = &text[new..];
        for i in 10..14 {
            assert!(body.contains(&format!("message {i}")), "{body}");
        }
        assert!(!body.contains("message 9"), "{body}");
        assert!(!text.contains("message 0"), "{text}");
    }

    #[test]
    fn reduce_from_overlap_keeps_text_only() {
        let mut first = String::new();
        first.push_str(&turn("user", 0));
        first.push_str(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"checking"},{"type":"tool_use","name":"Bash","input":{"command":"ls secret-dir"}}]}}"#);
        first.push('\n');
        let mut grown = first.clone();
        grown.push_str(&turn("user", 1));
        let d = reduce_bytes_from("x", grown.as_bytes(), first.len() as u64, 2, 6, 200_000);
        assert!(d.delta);
        assert!(d.text.contains("ASSISTANT: checking"), "{}", d.text);
        assert!(!d.text.contains("secret-dir"), "{}", d.text);
    }

    #[test]
    fn reduce_from_falls_back_when_mark_does_not_fit() {
        let t = ten();
        // Truncated below the mark.
        let short = &t[..t.len() / 2];
        let cut = short.rfind('\n').unwrap() + 1;
        let d = reduce_bytes_from(
            "x",
            &short.as_bytes()[..cut],
            t.len() as u64,
            11,
            6,
            200_000,
        );
        assert!(!d.delta);
        assert!(!d.text.contains(EARLIER_CONTEXT));
        assert!(d.text.contains("message 0"));
        assert_eq!(d.consumed_bytes, cut as u64);
        // Rewritten: same record count, offset no longer on that boundary.
        let mut other = t.replace("message 3", "message three");
        other.push_str(&turn("user", 10));
        let d = reduce_bytes_from("x", other.as_bytes(), t.len() as u64, 11, 6, 200_000);
        assert!(!d.delta);
        assert!(d.text.contains("message 0") && d.text.contains("message three"));
        // A mark past the record count.
        let d = reduce_bytes_from("x", t.as_bytes(), t.len() as u64, 12, 6, 200_000);
        assert!(!d.delta);
    }

    #[test]
    fn reduce_from_leaves_partial_tail_for_next_time() {
        let t = ten();
        let partial = format!("{t}{{\"type\":\"user\",\"mess");
        let d = reduce_bytes_from("x", partial.as_bytes(), 0, 0, 0, 200_000);
        assert_eq!(d.consumed_bytes, t.len() as u64);
        assert_eq!(d.consumed_records, 11);
        // A complete but unterminated last record is consumed; once its
        // newline arrives the mark still fits.
        let mut whole = t.clone();
        whole.push_str(turn("user", 10).trim_end());
        let d = reduce_bytes_from("x", whole.as_bytes(), 0, 0, 0, 200_000);
        assert_eq!(d.consumed_bytes, whole.len() as u64);
        assert_eq!(d.consumed_records, 12);
        let mut later = format!("{whole}\n");
        later.push_str(&turn("assistant", 11));
        let d2 = reduce_bytes_from("x", later.as_bytes(), d.consumed_bytes, 12, 1, 200_000);
        assert!(d2.delta, "{}", d2.text);
        assert!(d2.text.contains("message 11"));
        assert_eq!(d2.turns, 1);
    }

    #[test]
    fn reduce_from_reads_file_and_warns_on_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.jsonl");
        std::fs::write(&p, ten()).unwrap();
        let d = reduce_from(&p, 0, 0, 0, 200_000).unwrap();
        assert_eq!(d.session_id, "abc");
        let d2 = reduce_from(&p, d.consumed_bytes + 5, 11, 6, 200_000).unwrap();
        assert!(!d2.delta);
        assert_eq!(d2.text, d.text);
    }
}
