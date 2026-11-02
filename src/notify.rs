//! Best-effort desktop notification (macOS `osascript`).

/// AppleScript string literal.
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Show a notification. No-op off macOS or when `DREAMD_NO_NOTIFY=1`;
/// every error is ignored.
pub fn notify(title: &str, body: &str) {
    if std::env::var("DREAMD_NO_NOTIFY").is_ok_and(|v| v == "1") {
        return;
    }
    if !cfg!(target_os = "macos") {
        return;
    }
    let script = format!(
        "display notification {} with title {}",
        applescript_str(body),
        applescript_str(title)
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    #[test]
    fn applescript_quoting() {
        assert_eq!(super::applescript_str(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
