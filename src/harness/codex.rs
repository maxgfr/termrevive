//! Codex CLI: sessions are served by a shared `codex app-server` daemon, not
//! by the CLI process. Under `$CODEX_HOME` (`~/.codex`), every thread being
//! written holds `thread-writer-locks/<id>.lock`, and its rollout
//! (`sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`) starts with a `session_meta`
//! line carrying its `cwd` and creation time. A CLI process is matched to the
//! active threads of its directory, the one created closest to its start first.

use super::{AgentProc, Harness};
use crate::model::AgentSession;
use crate::procs::Proc;
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct Codex;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    timestamp: String,
}

/// An active thread: id, cwd, creation time (seconds since the epoch).
#[derive(Debug, Clone, PartialEq)]
struct Thread {
    id: String,
    cwd: String,
    created: Option<i64>,
}

fn home() -> Option<PathBuf> {
    super::home_dir_from("CODEX_HOME", ".codex")
}

fn active_ids(home: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join("thread-writer-locks")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".lock")?;
            (!id.starts_with('.')).then(|| id.to_string())
        })
        .collect()
}

/// Rollout files under `sessions/`, newest day first.
fn rollouts(home: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        paths.reverse();
        for p in paths {
            if depth < 3 && p.is_dir() {
                walk(&p, depth + 1, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&home.join("sessions"), 0, &mut out);
    out
}

fn read_meta(path: &Path) -> Option<Thread> {
    let file = std::fs::File::open(path).ok()?;
    let first = BufReader::new(file).lines().next()?.ok()?;
    let line: Line = serde_json::from_str(&first).ok()?;
    (line.kind == "session_meta").then(|| Thread {
        created: chrono::DateTime::parse_from_rfc3339(&line.payload.timestamp)
            .ok()
            .map(|t| t.timestamp()),
        id: line.payload.id,
        cwd: line.payload.cwd,
    })
}

fn active_threads(home: &Path) -> Vec<Thread> {
    let ids = active_ids(home);
    if ids.is_empty() {
        return Vec::new();
    }
    rollouts(home)
        .iter()
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            ids.iter().any(|id| name.ends_with(&format!("-{id}.jsonl")))
        })
        .filter_map(|p| read_meta(p))
        .collect()
}

/// Threads of `p`'s directory, the one created closest to `p`'s start first.
fn rank(threads: Vec<Thread>, p: &AgentProc) -> Vec<Thread> {
    let Some(cwd) = &p.cwd else {
        return Vec::new();
    };
    let mut matching: Vec<Thread> = threads.into_iter().filter(|t| &t.cwd == cwd).collect();
    matching.sort_by_key(|t| match (t.created, p.started) {
        (Some(c), Some(s)) => (c - s).abs(),
        _ => i64::MAX,
    });
    matching
}

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn is_cli(&self, p: &AgentProc) -> bool {
        p.name == "codex"
            && !p.has_arg(&[
                "app-server",
                "mcp-server",
                "mcp",
                "exec",
                "login",
                "logout",
                "completion",
                "--help",
                "-h",
            ])
    }

    fn sessions(&self, p: &AgentProc, _procs: &[Proc]) -> Vec<AgentSession> {
        let Some(home) = home() else {
            return Vec::new();
        };
        rank(active_threads(&home), p)
            .into_iter()
            .map(|t| AgentSession {
                harness: self.name().into(),
                session_id: t.id,
                cwd: t.cwd,
                name: None,
            })
            .collect()
    }

    fn resume_command(&self, s: &AgentSession) -> String {
        format!("codex resume {}", crate::rules::shell_quote(&s.session_id))
    }

    /// `codex resume <id>`: the first positional argument after `resume`,
    /// skipping the values of options that take one (`-c key=value`…).
    fn session_in_argv(&self, argv: &[String]) -> Option<String> {
        const TAKES_VALUE: &[&str] = &[
            "-c",
            "--config",
            "--enable",
            "--disable",
            "-m",
            "--model",
            "-p",
            "--profile",
            "-C",
            "--cd",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-i",
            "--image",
            "--add-dir",
            "--local-provider",
            "--remote",
            "--remote-auth-token-env",
        ];
        let at = argv.iter().position(|a| a == "resume")?;
        let mut rest = argv[at + 1..].iter();
        while let Some(a) = rest.next() {
            if TAKES_VALUE.contains(&a.as_str()) {
                rest.next();
            } else if !a.starts_with('-') {
                return Some(a.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(home: &Path, day: &str, id: &str, cwd: &str, ts: &str) {
        let dir = home.join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("rollout-2026-10-25T19-27-45-{id}.jsonl")),
            format!(
                "{{\"timestamp\":\"{ts}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"timestamp\":\"{ts}\"}}}}\n{{\"type\":\"event\"}}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn matches_active_threads_by_cwd_and_start_time() {
        let home = std::env::temp_dir().join(format!("termrevive-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
        for id in ["A", "B", "C"] {
            std::fs::write(home.join(format!("thread-writer-locks/{id}.lock")), "").unwrap();
        }
        std::fs::write(home.join("thread-writer-locks/.coordination.lock"), "").unwrap();
        rollout(&home, "2026/10/25", "A", "/w", "2026-10-25T18:27:45.000Z");
        rollout(&home, "2026/10/25", "B", "/w", "2026-10-25T18:40:00.000Z");
        rollout(
            &home,
            "2026/10/24",
            "C",
            "/other",
            "2026-10-24T10:00:00.000Z",
        );
        rollout(&home, "2026/10/24", "D", "/w", "2026-10-24T10:00:00.000Z"); // not active

        let threads = active_threads(&home);
        assert_eq!(threads.len(), 3);
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-25T18:39:50Z")
            .unwrap()
            .timestamp();
        let p = AgentProc {
            pid: 1,
            tty: "ttys001".into(),
            argv: vec!["codex".into(), "hi".into()],
            name: "codex".into(),
            cwd: Some("/w".into()),
            started: Some(started),
        };
        assert!(Codex.is_cli(&p));
        let ids: Vec<String> = rank(threads, &p).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["B", "A"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_daemon_is_not_the_cli() {
        let p = AgentProc {
            pid: 1,
            tty: "??".into(),
            argv: vec!["codex".into(), "app-server".into(), "--listen".into()],
            name: "codex".into(),
            cwd: None,
            started: None,
        };
        assert!(!Codex.is_cli(&p));
    }
}
