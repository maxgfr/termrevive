//! Optional LaunchAgents: periodic `save --auto` and `restore --auto` at login.

use crate::store;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SAVE_LABEL: &str = "com.termrevive.save";
pub const RESTORE_LABEL: &str = "com.termrevive.restore";

pub fn plist_path(label: &str) -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

pub fn installed(label: &str) -> bool {
    plist_path(label).is_ok_and(|p| p.exists())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(label: &str, exe: &Path, args: &[&str], trigger: &str, log: &Path) -> String {
    let mut argv = format!(
        "\t\t<string>{}</string>\n",
        xml_escape(&exe.display().to_string())
    );
    for a in args {
        argv += &format!("\t\t<string>{}</string>\n", xml_escape(a));
    }
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{argv}	</array>
{trigger}	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn bootout(label: &str) {
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{}/{label}", domain())])
        .output();
}

fn install_one(label: &str, body: &str) -> Result<PathBuf> {
    let path = plist_path(label)?;
    fs::create_dir_all(path.parent().unwrap())?;
    bootout(label);
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain()])
        .arg(&path)
        .output()
        .context("running launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(path)
}

/// The binary path the agents run. A Homebrew keg path
/// (`<prefix>/Cellar/termrevive/<version>/bin/termrevive`) is swapped for its
/// `<prefix>/bin` link, which survives `brew upgrade`.
fn stable_exe(exe: &Path) -> PathBuf {
    let s = exe.to_string_lossy();
    if let Some((prefix, rest)) = s.split_once("/Cellar/")
        && let Some(name) = rest.rsplit('/').next()
    {
        let link = Path::new(prefix).join("bin").join(name);
        if link.exists() || cfg!(test) {
            return link;
        }
    }
    exe.to_path_buf()
}

pub fn install(auto_save: bool, auto_restore: bool, interval: u64) -> Result<Vec<PathBuf>> {
    let exe = stable_exe(&std::env::current_exe()?.canonicalize()?);
    let logs = store::state_dir().join("logs");
    fs::create_dir_all(&logs)?;
    let mut done = Vec::new();
    if auto_save {
        let trigger = format!(
            "\t<key>StartInterval</key>\n\t<integer>{interval}</integer>\n\t<key>RunAtLoad</key>\n\t<true/>\n"
        );
        let body = plist(
            SAVE_LABEL,
            &exe,
            &["save", "--auto"],
            &trigger,
            &logs.join("save.log"),
        );
        done.push(install_one(SAVE_LABEL, &body)?);
    }
    if auto_restore {
        let trigger = "\t<key>RunAtLoad</key>\n\t<true/>\n";
        let body = plist(
            RESTORE_LABEL,
            &exe,
            &["restore", "--auto"],
            trigger,
            &logs.join("restore.log"),
        );
        done.push(install_one(RESTORE_LABEL, &body)?);
    }
    Ok(done)
}

pub fn uninstall() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for label in [SAVE_LABEL, RESTORE_LABEL] {
        let path = plist_path(label)?;
        bootout(label);
        if path.exists() {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_lists_every_argument() {
        let p = plist(
            SAVE_LABEL,
            Path::new("/bin/a&b"),
            &["save", "--auto"],
            "",
            Path::new("/tmp/l"),
        );
        assert!(p.contains(
            "<string>/bin/a&amp;b</string>\n\t\t<string>save</string>\n\t\t<string>--auto</string>"
        ));
    }

    #[test]
    fn homebrew_kegs_resolve_to_the_bin_link() {
        assert_eq!(
            stable_exe(Path::new(
                "/opt/homebrew/Cellar/termrevive/1.0.0/bin/termrevive"
            )),
            PathBuf::from("/opt/homebrew/bin/termrevive")
        );
        assert_eq!(
            stable_exe(Path::new("/Users/me/.cargo/bin/termrevive")),
            PathBuf::from("/Users/me/.cargo/bin/termrevive")
        );
    }

    #[test]
    fn plists_are_valid() {
        let trigger = "\t<key>StartInterval</key>\n\t<integer>120</integer>\n\t<key>RunAtLoad</key>\n\t<true/>\n";
        let p = plist(
            SAVE_LABEL,
            Path::new("/bin/x"),
            &["save"],
            trigger,
            Path::new("/tmp/l"),
        );
        let file =
            std::env::temp_dir().join(format!("termrevive-test-{}.plist", std::process::id()));
        fs::write(&file, p).unwrap();
        let out = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        let _ = fs::remove_file(&file);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
