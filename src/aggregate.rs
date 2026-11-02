//! The aggregator: one `claude -p` run with `cwd = out/` that folds the
//! per-session findings into the cloned memory dir.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::claude::{Claude, ClaudeError, ClaudeRequest, ClaudeResult};
use crate::config::AggCfg;
use crate::model::Project;

/// Tools the aggregator may use. Reads are unscoped (it reads
/// `../findings/` and `../context/`); writes are scoped to its cwd (`out/`)
/// because a bare `Write`/`Edit` allow rule permits any path even under
/// acceptEdits (live-probed). No Bash: deletions go through the
/// `.dreamd-delete` manifest that Rust applies after the run.
pub const AGGREGATOR_TOOLS: [&str; 5] = ["Read", "Glob", "Grep", "Write(./**)", "Edit(./**)"];

/// Permission mode for the aggregator: edits are auto-accepted inside its
/// cwd (`out/`) only; anything else is denied in non-interactive mode.
pub const AGGREGATOR_PERMISSION_MODE: &str = "acceptEdits";

/// The aggregator prompt: Anthropic's four consolidation phases, the
/// evidence rule, the file conventions, the CLAUDE.md rule and the summary
/// requirement, then operator instructions verbatim when given.
///
/// `context_files` are the names copied into `<dream_dir>/context/`
/// (`CLAUDE.md`, `AGENTS.md`); they are referenced as `../context/<name>`
/// relative to `out/`, never by a project path.
pub fn aggregator_prompt(
    project: &Project,
    context_files: &[&str],
    findings_files: &[PathBuf],
    instructions: Option<&str>,
) -> String {
    let has_claude_md = context_files.contains(&"CLAUDE.md");
    let mut context = if has_claude_md {
        "A read-only copy of the project's CLAUDE.md is at `../context/CLAUDE.md`.".to_string()
    } else {
        "The project has no CLAUDE.md; skip the reconcile step against it.".to_string()
    };
    for name in context_files.iter().filter(|n| **n != "CLAUDE.md") {
        context.push_str(&format!(
            "\nA read-only copy of the project's {name} is at `../context/{name}`."
        ));
    }
    let reconcile = if has_claude_md {
        "reconcile the memories against `../context/CLAUDE.md`. Never edit CLAUDE.md; when a memory conflicts with it, keep or annotate the memory and report the conflict in your summary instead. "
    } else {
        "(there is no CLAUDE.md, so skip reconciling against it). "
    };
    let findings = if findings_files.is_empty() {
        "(none)".to_string()
    } else {
        findings_files
            .iter()
            .map(|p| format!("- {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut s = format!(
        "You are consolidating (\"dreaming\") the auto-memory directory of the Claude Code project `{slug}`.\n\
The current directory is a CLONE of that memory directory: `MEMORY.md` is the index and every other `.md` file holds one fact. Edit the clone in place.\n\
{context}\n\
\n\
Per-session findings mined from recent transcripts (JSON: insights, stale, corrections; each cites evidence as a session id plus a quote):\n\
{findings}\n\
\n\
Work in four phases:\n\
\n\
Phase 1 — Orient: read `MEMORY.md` and every `.md` file in the current directory so you know what is already remembered.\n\
\n\
Phase 2 — Gather: read every findings file above and fold it in. Add a memory only when a findings file cites evidence for it (session id and quote); never invent a memory without a cited insight. Prefer editing an existing file over creating a new one. Apply `corrections` to the files they name and treat `stale` entries as candidates for deletion.\n\
\n\
Phase 3 — Consolidate: merge near-duplicate memories into one file, delete files that are contradicted or stale, and convert relative dates to absolute ones. Keep the convention: one fact per file, each file starting with YAML frontmatter that has `name`, `description` and `metadata.type` (one of user, feedback, project, reference).\n\
\n\
Deleting files: you have no shell and cannot remove files yourself. To delete memory files, use the Write tool to create `.dreamd-delete` in the current directory listing the files to delete, one bare filename per line (for example `old-fact.md`; no paths, never `MEMORY.md`). dreamd removes the listed files after you finish. Also drop their lines from `MEMORY.md`.\n\
\n\
Phase 4 — Prune and index: {reconcile}Rewrite `MEMORY.md` so every line points at an existing file in the current directory (one line per file: `- [Title](file.md) — hook`), no file is left unindexed, and no file listed in `.dreamd-delete` is indexed.\n\
\n\
Rules:\n\
- Never modify files outside the current directory. Only `.md` files here (and `.dreamd-delete`) may be written.\n\
- Never edit CLAUDE.md; report conflicts instead.\n\
- Do not delete a memory unless a finding or a contradiction justifies it.\n\
\n\
Finish with a summary of at most 300 words under a line `## Summary`: what you added, merged, deleted and corrected, and any CLAUDE.md conflicts.\n",
        slug = project.slug,
    );
    if let Some(text) = instructions.map(str::trim).filter(|t| !t.is_empty()) {
        s.push_str("\n## Operator instructions\n");
        s.push_str(text);
        s.push('\n');
    }
    s
}

/// Run the aggregator with `cwd = out_dir`, `acceptEdits` and the
/// cwd-scoped tool allowlist (no Bash, no extra dirs): it can write only
/// inside `out/`.
pub async fn run_aggregator(
    claude: &dyn Claude,
    cfg: &AggCfg,
    out_dir: &Path,
    prompt: String,
) -> Result<ClaudeResult, ClaudeError> {
    let mut req = ClaudeRequest::new(
        prompt,
        &cfg.model,
        &cfg.effort,
        Duration::from_secs(cfg.timeout_secs),
    );
    req.cwd = Some(out_dir.to_path_buf());
    req.permission_mode = Some(AGGREGATOR_PERMISSION_MODE.into());
    req.allowed_tools = AGGREGATOR_TOOLS.iter().map(|s| s.to_string()).collect();
    claude.run(req).await
}

/// The summary part of the aggregator's final text: everything after the
/// last `## Summary` line, or the whole text when there is none.
pub fn extract_summary(text: &str) -> String {
    let mut rest = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.trim() == "## Summary" {
            rest = Some(offset);
        }
    }
    match rest {
        Some(i) => text[i..].trim().to_string(),
        None => text.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn project() -> Project {
        Project {
            config_dir: "/c".into(),
            slug: "-tmp-proj".into(),
            cwd: Some("/tmp/proj".into()),
            memory_dir: "/c/projects/-tmp-proj/memory".into(),
        }
    }

    #[test]
    fn aggregator_prompt_contains_phases_and_instructions() {
        let files = vec![PathBuf::from("/d/findings/s1.json")];
        let p = aggregator_prompt(&project(), &["CLAUDE.md"], &files, Some("focus on X"));
        for needle in [
            "Phase 1",
            "Phase 2",
            "Phase 3",
            "Phase 4",
            "only when a findings file cites evidence",
            "Prefer editing an existing file",
            "one fact per file",
            "`name`, `description` and `metadata.type`",
            "every line points at an existing file",
            "Never modify files outside the current directory",
            "Never edit CLAUDE.md; report conflicts instead",
            "at most 300 words under a line `## Summary`",
            "/d/findings/s1.json",
            "../context/CLAUDE.md",
            "`.dreamd-delete`",
            "one bare filename per line",
        ] {
            assert!(p.contains(needle), "missing {needle:?}");
        }
        // No project path and no shell deletion.
        assert!(!p.contains("/tmp/proj"), "{p}");
        assert!(!p.contains("Bash") && !p.contains("`rm"), "{p}");
        assert!(p
            .trim_end()
            .ends_with("## Operator instructions\nfocus on X"));
        let none = aggregator_prompt(&project(), &[], &files, None);
        assert!(!none.contains("Operator instructions"));
        assert!(none.contains("no CLAUDE.md"));
        assert!(!none.contains("../context/CLAUDE.md"), "{none}");
        let agents = aggregator_prompt(&project(), &["CLAUDE.md", "AGENTS.md"], &files, None);
        assert!(agents.contains("../context/AGENTS.md"));
    }

    #[test]
    fn summary_extraction() {
        assert_eq!(extract_summary("work\n\n## Summary\nDid X.\n"), "Did X.");
        assert_eq!(extract_summary("just text\n"), "just text");
    }

    struct Capture(Mutex<Option<ClaudeRequest>>);

    #[async_trait::async_trait]
    impl Claude for Capture {
        async fn run(&self, req: ClaudeRequest) -> Result<ClaudeResult, ClaudeError> {
            *self.0.lock().unwrap() = Some(req);
            Err(ClaudeError::BadOutput("capture".into()))
        }
    }

    #[tokio::test]
    async fn run_aggregator_is_contained() {
        let c = Capture(Mutex::new(None));
        let _ = run_aggregator(&c, &AggCfg::default(), Path::new("/d/out"), "p".into()).await;
        let req = c.0.lock().unwrap().take().unwrap();
        assert_eq!(req.cwd.as_deref(), Some(Path::new("/d/out")));
        assert_eq!(req.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(
            req.allowed_tools,
            vec!["Read", "Glob", "Grep", "Write(./**)", "Edit(./**)"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert!(!req.allowed_tools.iter().any(|t| t.contains("Bash")));
        assert!(!req
            .allowed_tools
            .iter()
            .any(|t| t == "Write" || t == "Edit"));
        assert!(req.add_dirs.is_empty());
        assert_eq!(req.model, "opus");
        assert!(req.json_schema.is_none());
    }
}
