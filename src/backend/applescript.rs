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

/// Runs `script` and returns the non-empty lines of its result.
pub fn lines(script: &str) -> Result<Vec<String>> {
    Ok(run(script)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// The script closing the windows of `app` that have these ids, if any. Only
/// numeric ids are used: they go into the script as they are.
fn close_script(app: &str, ids: &[String]) -> Option<String> {
    let body: String = ids
        .iter()
        .filter(|id| id.parse::<i64>().is_ok())
        .map(|id| format!("\ttry\n\t\tclose (first window whose id is {id})\n\tend try\n"))
        .collect();
    (!body.is_empty()).then(|| format!("tell application {}\n{body}end tell", str(app)))
}

/// Closes the windows of `app` that have these (numeric) ids.
pub fn close_windows(app: &str, ids: &[String]) -> Result<()> {
    match close_script(app, ids) {
        Some(script) => run(&script).map(drop),
        None => Ok(()),
    }
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
    fn closes_windows_by_numeric_id_only() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            close_script("iTerm2", &ids(&["12", "x\" & quit", "7"])).unwrap(),
            "tell application \"iTerm2\"\n\
             \ttry\n\t\tclose (first window whose id is 12)\n\tend try\n\
             \ttry\n\t\tclose (first window whose id is 7)\n\tend try\n\
             end tell"
        );
        assert_eq!(close_script("Terminal", &ids(&["/dev/ttys001"])), None);
        assert_eq!(close_script("Terminal", &[]), None);
    }

    #[test]
    fn splits_records() {
        let text = format!("W{US}1{RS}T{US}true{US}{RS}");
        let r: Vec<Vec<&str>> = records(&text).collect();
        assert_eq!(r, vec![vec!["W", "1"], vec!["T", "true", ""]]);
    }
}
