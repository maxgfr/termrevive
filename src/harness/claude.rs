//! Claude Code: every live session registers `~/.claude/sessions/<pid>.json`
//! (`$CLAUDE_CONFIG_DIR/sessions`), keyed by the CLI's own pid.

use super::{AgentProc, Harness};
use crate::model::AgentSession;
use crate::procs::Proc;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct Claude;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    session_id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
    /// When the CLI registered, in ms since the epoch: right after it started.
    #[serde(default)]
    started_at: Option<i64>,
}

/// A CLI that is killed leaves its entry behind, and its pid is reused sooner
/// or later: an entry written more than this long before the process started
/// belongs to a previous owner of the pid.
const STALE_SECS: i64 = 60;

fn sessions_dir() -> Option<PathBuf> {
    Some(super::home_dir_from("CLAUDE_CONFIG_DIR", ".claude")?.join("sessions"))
}

/// The entry `p` registered in `dir`.
fn registered_in(dir: &Path, p: &AgentProc) -> Option<SessionFile> {
    let bytes = std::fs::read(dir.join(format!("{}.json", p.pid))).ok()?;
    let entry: SessionFile = serde_json::from_slice(&bytes).ok()?;
    let stale = matches!(
        (entry.started_at, p.started),
        (Some(at), Some(started)) if at / 1000 + STALE_SECS < started
    );
    (!stale).then_some(entry)
}

fn registered(p: &AgentProc) -> Option<SessionFile> {
    registered_in(&sessions_dir()?, p)
}

fn session(entry: SessionFile) -> AgentSession {
    AgentSession {
        harness: "claude".into(),
        session_id: entry.session_id,
        cwd: entry.cwd,
        name: entry.name.filter(|n| !n.is_empty()),
    }
}

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn is_cli(&self, p: &AgentProc) -> bool {
        p.name == "claude" || registered(p).is_some()
    }

    fn sessions(&self, p: &AgentProc, _procs: &[Proc]) -> Vec<AgentSession> {
        registered(p).map(session).into_iter().collect()
    }

    fn resume_command(&self, s: &AgentSession) -> String {
        format!(
            "claude --resume {}",
            crate::rules::shell_quote(&s.session_id)
        )
    }

    fn session_in_argv(&self, argv: &[String]) -> Option<String> {
        super::flag_value(argv, &["--resume", "-r", "--session-id"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: i32, started: Option<i64>) -> AgentProc {
        AgentProc {
            pid,
            tty: "ttys001".into(),
            argv: vec!["node".into()],
            name: "node".into(),
            cwd: None,
            started,
        }
    }

    #[test]
    fn reads_the_session_registered_for_the_pid() {
        let dir = std::env::temp_dir().join(format!("termrevive-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("4242.json"),
            r#"{"pid":4242,"sessionId":"abc","cwd":"/w","name":"fix-bug","status":"idle"}"#,
        )
        .unwrap();
        let s = session(registered_in(&dir, &proc(4242, Some(1_000))).unwrap());
        assert_eq!(s.session_id, "abc");
        assert_eq!(s.cwd, "/w");
        assert_eq!(s.name.as_deref(), Some("fix-bug"));
        assert_eq!(Claude.resume_command(&s), "claude --resume abc");
        assert!(registered_in(&dir, &proc(4243, None)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_older_than_the_process_is_a_reused_pid() {
        let dir =
            std::env::temp_dir().join(format!("termrevive-claude-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Registered at t = 1000 s.
        std::fs::write(
            dir.join("4242.json"),
            r#"{"pid":4242,"sessionId":"old","cwd":"/w","startedAt":1000000}"#,
        )
        .unwrap();
        // The CLI registers a moment after it starts: this is its own entry.
        assert!(registered_in(&dir, &proc(4242, Some(998))).is_some());
        // A process started an hour later only shares the pid.
        assert!(registered_in(&dir, &proc(4242, Some(4_600))).is_none());
        // Nothing to compare: the entry is trusted, as before.
        assert!(registered_in(&dir, &proc(4242, None)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
