//! Mistral Vibe: a live session holds `logs/session/active/<id>.lock.json`
//! (`{"process_id", "session_id"}`) under `$VIBE_HOME` (`~/.vibe`). The pid is
//! the CLI's `vibe-app-server` helper, on the same tty as the CLI.
//! `logs/session/unified/<id>/meta.json` holds the directory and title.

use super::{AgentProc, Harness};
use crate::model::AgentSession;
use crate::procs::Proc;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct Vibe;

#[derive(Deserialize)]
struct Lock {
    process_id: i32,
    session_id: String,
}

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    environment: Env,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize, Default)]
struct Env {
    #[serde(default)]
    working_directory: String,
}

fn session_root() -> Option<PathBuf> {
    Some(super::home_dir_from("VIBE_HOME", ".vibe")?.join("logs/session"))
}

fn sessions_in(root: &Path, p: &AgentProc, procs: &[Proc]) -> Vec<AgentSession> {
    let Ok(entries) = std::fs::read_dir(root.join("active")) else {
        return Vec::new();
    };
    let on_this_tty = |pid: i32| {
        pid == p.pid
            || procs
                .iter()
                .any(|q| q.pid == pid && q.tty == p.tty && q.tty != "??")
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".lock.json"))
        .filter_map(|e| serde_json::from_slice::<Lock>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|l| on_this_tty(l.process_id))
        .map(|l| {
            let meta: Meta =
                std::fs::read(root.join("unified").join(&l.session_id).join("meta.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
            AgentSession {
                harness: "vibe".into(),
                session_id: l.session_id,
                cwd: meta.environment.working_directory,
                name: meta.title.filter(|t| !t.is_empty()),
            }
        })
        .collect()
}

impl Harness for Vibe {
    fn name(&self) -> &'static str {
        "vibe"
    }

    fn display_name(&self) -> &'static str {
        "Mistral Vibe"
    }

    fn is_cli(&self, p: &AgentProc) -> bool {
        matches!(p.name.as_str(), "vibe" | "vibe-rs") && !p.has_arg(&["mcp", "--help", "-h"])
    }

    fn sessions(&self, p: &AgentProc, procs: &[Proc]) -> Vec<AgentSession> {
        session_root()
            .map(|root| sessions_in(&root, p, procs))
            .unwrap_or_default()
    }

    fn resume_command(&self, s: &AgentSession) -> String {
        format!("vibe --resume {}", crate::rules::shell_quote(&s.session_id))
    }

    fn session_in_argv(&self, argv: &[String]) -> Option<String> {
        super::flag_value(argv, &["--resume"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_lock_held_by_a_helper_on_the_same_tty() {
        let root = std::env::temp_dir().join(format!("termrevive-vibe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("active")).unwrap();
        std::fs::create_dir_all(root.join("unified/S1")).unwrap();
        std::fs::write(
            root.join("active/S1.lock.json"),
            r#"{"acquired_at":"x","lease_version":1,"process_id":201,"session_id":"S1"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("active/S2.lock.json"),
            r#"{"process_id":999,"session_id":"S2"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("unified/S1/meta.json"),
            r#"{"session_id":"S1","environment":{"working_directory":"/w"},"title":"plan"}"#,
        )
        .unwrap();
        let helper = Proc {
            pid: 201,
            ppid: 200,
            pgid: 200,
            tpgid: 200,
            tty: "ttys010".into(),
            comm: "vibe-app-server".into(),
        };
        let cli = AgentProc {
            pid: 200,
            tty: "ttys010".into(),
            argv: vec!["/x/vibe-rs".into()],
            name: "vibe-rs".into(),
            cwd: Some("/w".into()),
            started: None,
        };
        assert!(Vibe.is_cli(&cli));
        let s = sessions_in(&root, &cli, &[helper]);
        assert_eq!(s.len(), 1, "the stale lock (pid 999) is ignored");
        assert_eq!(
            (
                s[0].session_id.as_str(),
                s[0].cwd.as_str(),
                s[0].name.as_deref()
            ),
            ("S1", "/w", Some("plan"))
        );
        assert_eq!(Vibe.resume_command(&s[0]), "vibe --resume S1");
        let _ = std::fs::remove_dir_all(&root);
    }
}
