//! Helpers for backends driven through AppleScript (`osascript`).

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Field and record separators used by the read scripts.
pub const US: char = '\u{1f}';
pub const RS: char = '\u{1e}';

/// AppleScript handler turning `missing value` and non-text into text; scripts
/// call it as `my txt(…)`.
pub const TXT_HANDLER: &str = r#"
on txt(v)
	try
		if v is missing value then return ""
		return v as text
	on error
		return ""
	end try
end txt
"#;

/// Runs `script` and returns its result, without the trailing newline.
pub fn run(script: &str) -> Result<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running osascript")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-1743") {
            bail!(
                "macOS refused to let termrevive control this app. Allow it in System Settings → \
                 Privacy & Security → Automation, then retry.\n{}",
                err.trim()
            );
        }
        bail!("osascript failed: {}", err.trim());
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.ends_with('\n') {
        s.pop();
    }
    Ok(s)
}

/// An AppleScript string expression for `s`.
pub fn str(s: &str) -> String {
    let body = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\" & return & \"")
        .replace('\n', "\" & linefeed & \"");
    format!("\"{body}\"")
}

/// `delay` statement for `ms` milliseconds.
pub fn delay(ms: u64) -> String {
    format!("\tdelay {:.3}\n", ms as f64 / 1000.0)
}

/// Starts the app with this bundle id (no-op if it runs).
pub fn launch(bundle_id: &str) -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-b", bundle_id])
        .status()
        .with_context(|| format!("launching {bundle_id}"))?;
    if !status.success() {
        bail!("`open -b {bundle_id}` failed");
    }
    Ok(())
}

/// Retries `probe` until it succeeds, for up to a minute.
pub fn wait_ready(probe: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match run(probe) {
            Ok(_) => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Splits a read script's output into records of fields.
pub fn records(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.split(RS)
        .filter(|r| !r.is_empty())
        .map(|r| r.split(US).collect())
}

/// Whether an app bundle with this name is installed.
pub fn app_installed(app: &str) -> bool {
    let home = dirs::home_dir().unwrap_or_default();
    [
        std::path::PathBuf::from("/Applications"),
        std::path::PathBuf::from("/System/Applications/Utilities"),
        home.join("Applications"),
    ]
    .iter()
    .any(|d| d.join(app).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_strings() {
        assert_eq!(str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(str("x\ny"), "\"x\" & linefeed & \"y\"");
    }

    #[test]
    fn splits_records() {
        let text = format!("W{US}1{RS}T{US}true{US}{RS}");
        let r: Vec<Vec<&str>> = records(&text).collect();
        assert_eq!(r, vec![vec!["W", "1"], vec!["T", "true", ""]]);
    }
}
