//! The optional LaunchAgent: one long-lived `termrevive agent`, started once at
//! login, which restores (when enabled) and then saves in a loop. A job that
//! launchd starts again every few minutes gets re-registered by macOS'
//! Background Task Management on each launch, so it stays a single process.

use crate::store;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const AGENT_LABEL: &str = "com.termrevive.agent";
/// The agents of termrevive ≤ 1.2 (`save --auto` every 2 minutes, `restore
/// --auto` at login), replaced by [`AGENT_LABEL`].
pub const LEGACY_LABELS: [&str; 2] = ["com.termrevive.save", "com.termrevive.restore"];

/// What the installed agent does.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub restore: bool,
    pub save_every: Option<u64>,
}

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

/// Arguments of `termrevive agent` for `config`.
fn agent_args(config: &AgentConfig) -> Vec<String> {
    let mut args = vec!["agent".to_string()];
    if config.restore {
        args.push("--restore".into());
    }
    if let Some(every) = config.save_every {
        args.push("--save-every".into());
        args.push(every.to_string());
    }
    args
}

fn parse_agent_args(args: &[String]) -> Option<AgentConfig> {
    let at = args.iter().position(|a| a == "agent")?;
    let rest = &args[at + 1..];
    Some(AgentConfig {
        restore: rest.iter().any(|a| a == "--restore"),
        save_every: rest
            .iter()
            .position(|a| a == "--save-every")
            .and_then(|i| rest.get(i + 1))
            .and_then(|v| v.parse().ok()),
    })
}

/// The installed agent's configuration, read back from its plist.
pub fn agent_config() -> Option<AgentConfig> {
    let path = plist_path(AGENT_LABEL).ok().filter(|p| p.exists())?;
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", "ProgramArguments", "json", "-o", "-"])
        .arg(&path)
        .output()
        .ok()?;
    let args: Vec<String> = serde_json::from_slice(&out.stdout).ok()?;
    parse_agent_args(&args)
}

/// Whether something restores at login: the agent, or a legacy restore agent.
pub fn restores_at_login() -> bool {
    agent_config().is_some_and(|c| c.restore) || installed(LEGACY_LABELS[1])
}

/// Legacy agents still installed.
pub fn legacy_installed() -> Vec<&'static str> {
    LEGACY_LABELS.into_iter().filter(|l| installed(l)).collect()
}

fn remove(label: &str) -> Result<Option<PathBuf>> {
    let path = plist_path(label)?;
    bootout(label);
    if path.exists() {
        fs::remove_file(&path)?;
        return Ok(Some(path));
    }
    Ok(None)
}

/// Installs the agent (replacing any previous one, legacy agents included).
/// Returns the plist written and the legacy plists removed.
pub fn install(config: &AgentConfig) -> Result<(PathBuf, Vec<PathBuf>)> {
    let mut removed = Vec::new();
    for label in LEGACY_LABELS {
        removed.extend(remove(label)?);
    }
    let exe = stable_exe(&std::env::current_exe()?.canonicalize()?);
    let logs = store::state_dir().join("logs");
    fs::create_dir_all(&logs)?;
    // Started once at login; restarted only if it crashes.
    let trigger = "\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n";
    let args = agent_args(config);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let body = plist(AGENT_LABEL, &exe, &refs, trigger, &logs.join("agent.log"));
    Ok((install_one(AGENT_LABEL, &body)?, removed))
}

/// Removes the agent and any legacy agent.
pub fn uninstall() -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for label in std::iter::once(AGENT_LABEL).chain(LEGACY_LABELS) {
        removed.extend(remove(label)?);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_lists_every_argument() {
        let p = plist(
            AGENT_LABEL,
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
    fn agent_arguments_round_trip() {
        for config in [
            AgentConfig {
                restore: true,
                save_every: Some(120),
            },
            AgentConfig {
                restore: false,
                save_every: Some(60),
            },
            AgentConfig {
                restore: true,
                save_every: None,
            },
        ] {
            let mut args = vec!["/opt/homebrew/bin/termrevive".to_string()];
            args.extend(agent_args(&config));
            assert_eq!(parse_agent_args(&args), Some(config));
        }
        assert_eq!(
            parse_agent_args(&["termrevive".into(), "save".into()]),
            None
        );
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
        let trigger = "\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n";
        let p = plist(
            AGENT_LABEL,
            Path::new("/bin/x"),
            &["agent", "--restore", "--save-every", "120"],
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
