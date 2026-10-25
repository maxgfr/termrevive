//! Agent CLIs (harnesses): Claude Code, Codex, Mistral Vibe, opencode, Kilo…
//! A harness recognizes its interactive CLI among the processes of a pane,
//! finds the session that process works on, and says how to resume it.
//!
//! Adding a harness: implement [`Harness`] in its own module, list it in
//! [`registry`], and cover its session lookup with fixture tests (see
//! CONTRIBUTING.md). Like terminal backends, everything else — capture,
//! pairing, rules, snapshots, restore — is shared.

pub mod claude;
pub mod codex;
pub mod sqlite;
pub mod vibe;

use crate::model::AgentSession;
use crate::procs::{self, Proc};
use std::path::PathBuf;

/// A process that may be a harness's CLI, with what lookups need.
#[derive(Debug, Clone)]
pub struct AgentProc {
    pub pid: i32,
    pub tty: String,
    pub argv: Vec<String>,
    /// Basename of `argv[0]`, without a leading `-`.
    pub name: String,
    pub cwd: Option<String>,
    /// Seconds since the epoch.
    pub started: Option<i64>,
}

impl AgentProc {
    pub fn new(p: &Proc, cwd: Option<String>) -> AgentProc {
        let argv = procs::argv(p.pid).unwrap_or_else(|| vec![p.comm.clone()]);
        AgentProc {
            pid: p.pid,
            tty: p.tty.clone(),
            name: procs::basename(&argv[0]).to_string(),
            argv,
            cwd,
            started: procs::start_time(p.pid),
        }
    }

    /// Whether `argv` has any of `words` past the program name (subcommands).
    pub fn has_arg(&self, words: &[&str]) -> bool {
        self.argv
            .iter()
            .skip(1)
            .any(|a| words.contains(&a.as_str()))
    }
}

pub trait Harness {
    /// Stable id, stored in snapshots and matched by `[[rule]] match`.
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// Whether `p` is this harness's interactive CLI.
    fn is_cli(&self, p: &AgentProc) -> bool;
    /// Sessions `p` may be working on, the likeliest first. `procs` is the
    /// whole process table (for registries keyed by a helper process's pid).
    fn sessions(&self, p: &AgentProc, procs: &[Proc]) -> Vec<AgentSession>;
    /// What to type in the pane's shell to resume `s`. Aliases apply.
    fn resume_command(&self, s: &AgentSession) -> String;
    /// The command that starts the CLI, put back in place of the executable's
    /// path when the session is unknown (`…/Cellar/x/1.2/bin/vibe-rs` → `vibe`),
    /// so it survives upgrades and goes through the user's aliases.
    fn command(&self) -> &'static str {
        self.name()
    }
    /// The session id the CLI was started with (`--resume <id>`…), used when
    /// `sessions` finds nothing — e.g. a session resumed but not used yet.
    fn session_in_argv(&self, _argv: &[String]) -> Option<String> {
        None
    }
}

/// The value after the first of `flags` in `argv` (`--session X` or
/// `--session=X`).
pub fn flag_value(argv: &[String], flags: &[&str]) -> Option<String> {
    argv.iter().enumerate().skip(1).find_map(|(i, a)| {
        flags.iter().find_map(|f| {
            if a == f {
                argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
            } else {
                a.strip_prefix(&format!("{f}=")).map(String::from)
            }
        })
    })
}

/// Every supported harness.
pub fn registry() -> Vec<Box<dyn Harness>> {
    vec![
        Box::new(claude::Claude),
        Box::new(codex::Codex),
        Box::new(vibe::Vibe),
        Box::new(sqlite::opencode()),
        Box::new(sqlite::kilo()),
    ]
}

pub fn by_name(name: &str) -> Option<Box<dyn Harness>> {
    registry().into_iter().find(|h| h.name() == name)
}

pub fn display(name: &str) -> String {
    by_name(name)
        .map(|h| h.display_name().to_string())
        .unwrap_or_else(|| name.to_string())
}

/// `$VAR`, else `~/<default>`.
pub fn home_dir_from(var: &str, default: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(dirs::home_dir()?.join(default)),
    }
}

/// `$XDG_DATA_HOME/<app>`, else `~/.local/share/<app>`.
pub fn xdg_data(app: &str) -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d).join(app)),
        _ => Some(dirs::home_dir()?.join(".local/share").join(app)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn every_harness_reads_the_session_it_was_started_with() {
        let cases = [
            ("claude", "claude --resume abc --verbose", Some("abc")),
            ("claude", "claude -r abc", Some("abc")),
            ("codex", "codex resume -c x=1 019a", Some("019a")),
            ("codex", "codex hello", None),
            ("vibe", "/x/vibe-rs --resume d75b", Some("d75b")),
            ("opencode", "opencode --session=ses_1", Some("ses_1")),
            ("kilo", "/x/libexec/kilo -s ses_2", Some("ses_2")),
            ("kilo", "kilo --session --fork", None),
        ];
        for (h, line, want) in cases {
            let got = by_name(h).unwrap().session_in_argv(&argv(line));
            assert_eq!(got.as_deref(), want, "{h}: {line}");
        }
    }
}
