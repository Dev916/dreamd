//! Nightly launchd job (`dev.dreamd.nightly`): render the plist, install /
//! uninstall it under `~/Library/LaunchAgents/`, report its status.
//! `launchctl` failures are printed warnings, never errors; under
//! `DREAMD_SKIP_LAUNCHCTL=1` launchctl is not invoked at all (tests).

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{Config, ScheduleCfg};

pub const LABEL: &str = "dev.dreamd.nightly";

/// What `schedule status` reports. `hour`/`minute` come from the installed
/// plist (the config defaults when it is not installed).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScheduleStatus {
    pub installed: bool,
    pub hour: u8,
    pub minute: u8,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd plist running `<exe> dream --all` daily at `cfg.hour:cfg.minute`,
/// logging to `<log_dir>/nightly.{out,err}.log`.
pub fn plist_xml(exe: &Path, cfg: &ScheduleCfg, log_dir: &Path) -> String {
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let e = |p: &Path| xml_escape(&p.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `dreamd schedule install`. launchd runs with a minimal
     environment, so PATH is captured from the installing shell. claude_bin in
     the dreamd config must be the REAL claude binary, not a wrapper that adds
     dangerously-skip-permissions (see the Security model in the dreamd spec);
     dreamd refuses to run against such a wrapper. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>dream</string>
    <string>--all</string>
  </array>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key><integer>{hour}</integer>
    <key>Minute</key><integer>{minute}</integer>
  </dict>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
  <key>RunAtLoad</key>
  <false/>
</dict>
</plist>
"#,
        exe = e(exe),
        hour = cfg.hour,
        minute = cfg.minute,
        path = xml_escape(&path_env),
        out = e(&log_dir.join("nightly.out.log")),
        err = e(&log_dir.join("nightly.err.log")),
    )
}

fn home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// `$HOME/Library/LaunchAgents/dev.dreamd.nightly.plist`.
pub fn plist_path() -> PathBuf {
    home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// `<data_dir>/logs`.
pub fn log_dir(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("logs")
}

fn skip_launchctl() -> bool {
    std::env::var("DREAMD_SKIP_LAUNCHCTL").is_ok_and(|v| v == "1")
}

/// Run `launchctl <args>`; any failure is a warning on stderr.
fn launchctl(args: &[&str]) {
    if skip_launchctl() {
        return;
    }
    match std::process::Command::new("launchctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "warning: launchctl {} failed ({}): {}",
            args.join(" "),
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("warning: launchctl {}: {e}", args.join(" ")),
    }
}

fn gui_domain() -> String {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "501".into());
    format!("gui/{uid}")
}

/// Write the plist (and the log dir) and bootstrap it. `dry_run` prints the
/// target path and plist instead.
pub fn install(cfg: &Config, dry_run: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let logs = log_dir(cfg);
    let xml = plist_xml(&exe, &cfg.schedule, &logs);
    let path = plist_path();
    if dry_run {
        println!("would write {}", path.display());
        print!("{xml}");
        return Ok(());
    }
    std::fs::create_dir_all(&logs).with_context(|| format!("create {}", logs.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let domain = gui_domain();
    if path.exists() {
        // Replace a previous install: unload it first (best effort).
        launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    }
    std::fs::write(&path, xml).with_context(|| format!("write {}", path.display()))?;
    launchctl(&["bootstrap", &domain, &path.display().to_string()]);
    println!(
        "installed {} (daily at {:02}:{:02}, logs in {})",
        path.display(),
        cfg.schedule.hour,
        cfg.schedule.minute,
        logs.display()
    );
    Ok(())
}

/// Boot out the job and remove the plist. Not installed is not an error.
pub fn uninstall() -> anyhow::Result<()> {
    let path = plist_path();
    if !path.exists() {
        println!("not installed");
        return Ok(());
    }
    launchctl(&["bootout", &format!("{}/{LABEL}", gui_domain())]);
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    println!("removed {}", path.display());
    Ok(())
}

/// The integer following `<key>{key}</key>` in a plist.
fn plist_int(xml: &str, key: &str) -> Option<u8> {
    let after = &xml[xml.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<integer>")? + "<integer>".len();
    let end = after[start..].find("</integer>")? + start;
    after[start..end].trim().parse().ok()
}

/// Whether the plist exists, and the time it schedules.
pub fn status() -> anyhow::Result<ScheduleStatus> {
    let d = ScheduleCfg::default();
    let path = plist_path();
    if !path.exists() {
        return Ok(ScheduleStatus {
            installed: false,
            hour: d.hour,
            minute: d.minute,
        });
    }
    let xml = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(ScheduleStatus {
        installed: true,
        hour: plist_int(&xml, "Hour").unwrap_or(d.hour),
        minute: plist_int(&xml, "Minute").unwrap_or(d.minute),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_schedule_and_exe() {
        let cfg = ScheduleCfg {
            hour: 4,
            minute: 5,
            auto_apply: false,
        };
        let xml = plist_xml(Path::new("/opt/bin/dreamd"), &cfg, Path::new("/data/logs"));
        for needle in [
            "<string>dev.dreamd.nightly</string>",
            "<string>/opt/bin/dreamd</string>",
            "<string>dream</string>",
            "<string>--all</string>",
            "<key>Hour</key><integer>4</integer>",
            "<key>Minute</key><integer>5</integer>",
            "<key>StandardOutPath</key>\n  <string>/data/logs/nightly.out.log</string>",
            "<key>StandardErrorPath</key>\n  <string>/data/logs/nightly.err.log</string>",
            "<key>PATH</key>",
            "claude_bin",
        ] {
            assert!(xml.contains(needle), "missing {needle:?} in\n{xml}");
        }
        assert_eq!(plist_int(&xml, "Hour"), Some(4));
        assert_eq!(plist_int(&xml, "Minute"), Some(5));
    }

    #[test]
    fn plist_escapes_xml() {
        let xml = plist_xml(
            Path::new("/a&b/<d>"),
            &ScheduleCfg::default(),
            Path::new("/l"),
        );
        assert!(xml.contains("<string>/a&amp;b/&lt;d&gt;</string>"), "{xml}");
    }
}
