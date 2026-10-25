//! Claude Code: every live session registers `~/.claude/sessions/<pid>.json`
//! (`$CLAUDE_CONFIG_DIR/sessions`), keyed by the CLI's own pid.

use super::{AgentProc, Harness};
use crate::model::AgentSession;
use crate::procs::Proc;
use serde::Deserialize;
use std::path::PathBuf;

pub struct Claude;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    session_id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
}

fn sessions_dir() -> Option<PathBuf> {
    Some(super::home_dir_from("CLAUDE_CONFIG_DIR", ".claude")?.join("sessions"))
}

fn registered(pid: i32) -> Option<SessionFile> {
    let bytes = std::fs::read(sessions_dir()?.join(format!("{pid}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn is_cli(&self, p: &AgentProc) -> bool {
        p.name == "claude" || registered(p.pid).is_some()
    }

    fn sessions(&self, p: &AgentProc, _procs: &[Proc]) -> Vec<AgentSession> {
        registered(p.pid)
            .map(|s| AgentSession {
                harness: self.name().into(),
                session_id: s.session_id,
                cwd: s.cwd,
                name: s.name.filter(|n| !n.is_empty()),
            })
            .into_iter()
            .collect()
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

    #[test]
    fn reads_the_session_registered_for_the_pid() {
        let dir = std::env::temp_dir().join(format!("termrevive-claude-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions/4242.json"),
            r#"{"pid":4242,"sessionId":"abc","cwd":"/w","name":"fix-bug","status":"idle"}"#,
        )
        .unwrap();
        // SAFETY: tests touching CLAUDE_CONFIG_DIR run in this one test only.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };
        let p = AgentProc {
            pid: 4242,
            tty: "ttys001".into(),
            argv: vec!["node".into()],
            name: "node".into(),
            cwd: None,
            started: None,
        };
        assert!(Claude.is_cli(&p));
        let s = Claude.sessions(&p, &[]);
        assert_eq!(s[0].session_id, "abc");
        assert_eq!(s[0].name.as_deref(), Some("fix-bug"));
        assert_eq!(Claude.resume_command(&s[0]), "claude --resume abc");
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
