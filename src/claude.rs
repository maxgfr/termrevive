//! Live Claude Code sessions, as registered in `~/.claude/sessions/<pid>.json`.

use crate::model::ClaudeSession;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: i32,
    session_id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
}

fn sessions_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir()?.join(".claude"),
    };
    Some(base.join("sessions"))
}

/// Sessions keyed by PID, restricted to PIDs that are still alive.
pub fn live_sessions(alive: impl Fn(i32) -> bool) -> HashMap<i32, ClaudeSession> {
    let Some(entries) = sessions_dir().and_then(|d| std::fs::read_dir(d).ok()) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<SessionFile>(&bytes).ok())
        .filter(|s| alive(s.pid))
        .map(|s| {
            (
                s.pid,
                ClaudeSession {
                    session_id: s.session_id,
                    cwd: s.cwd,
                    name: s.name.filter(|n| !n.is_empty()),
                },
            )
        })
        .collect()
}
