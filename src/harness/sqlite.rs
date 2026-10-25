//! Harnesses that keep sessions in an opencode-style SQLite database: opencode
//! itself and its fork Kilo. Sessions live in `session_v2` (opencode ≥ 2) and/or
//! `session`, with the `directory` they belong to and a `time_updated` in ms.
//! A CLI process is matched to the top-level sessions of its directory that
//! were active during its lifetime, the most recent first. The database is read
//! with macOS' own `sqlite3`, read-only.

use super::{AgentProc, Harness};
use crate::model::AgentSession;
use crate::procs::Proc;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct SqliteHarness {
    name: &'static str,
    display: &'static str,
    bins: &'static [&'static str],
    db: fn() -> Option<PathBuf>,
    resume_flag: &'static str,
}

pub fn opencode() -> SqliteHarness {
    SqliteHarness {
        name: "opencode",
        display: "opencode",
        bins: &["opencode"],
        db: || Some(super::xdg_data("opencode")?.join("opencode.db")),
        resume_flag: "--session",
    }
}

pub fn kilo() -> SqliteHarness {
    SqliteHarness {
        name: "kilo",
        display: "Kilo",
        bins: &["kilo", "kilocode"],
        db: || Some(super::xdg_data("kilo")?.join("kilo.db")),
        resume_flag: "--session",
    }
}

/// Subcommands that are not the interactive TUI.
const NOT_TUI: &[&str] = &[
    "serve",
    "run",
    "session",
    "export",
    "import",
    "auth",
    "mcp",
    "upgrade",
    "uninstall",
    "models",
    "stats",
    "web",
    "acp",
    "debug",
    "agent",
    "github",
    "remote",
    "--help",
    "-h",
    "--version",
];

#[derive(Deserialize)]
struct Row {
    id: String,
    directory: String,
    #[serde(default)]
    title: Option<String>,
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn query(db: &Path, sql: &str) -> Option<String> {
    let uri = format!("file:{}?mode=ro", db.display());
    let out = Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-json", &uri, sql])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Top-level sessions of `cwd` updated since `since_ms`, most recent first.
fn sessions_in(db: &Path, cwd: &str, since_ms: Option<i64>) -> Vec<(String, Option<String>)> {
    let tables = query(
        db,
        "select name from sqlite_master where type = 'table' and name in ('session_v2', 'session')",
    )
    .unwrap_or_default();
    let since = since_ms
        .map(|ms| format!(" and time_updated >= {ms}"))
        .unwrap_or_default();
    let selects: Vec<String> = ["session_v2", "session"]
        .iter()
        .filter(|t| tables.contains(&format!("\"{t}\"")))
        .map(|t| {
            format!(
                "select id, directory, title, time_updated from {t} \
                 where directory = {} and parent_id is null{since}",
                sql_str(cwd)
            )
        })
        .collect();
    if selects.is_empty() {
        return Vec::new();
    }
    let sql = format!(
        "select id, directory, title from ({}) order by time_updated desc",
        selects.join(" union all ")
    );
    let rows: Vec<Row> = query(db, &sql)
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|r| r.directory == cwd && seen.insert(r.id.clone()))
        .map(|r| (r.id, r.title.filter(|t| !t.is_empty())))
        .collect()
}

impl Harness for SqliteHarness {
    fn name(&self) -> &'static str {
        self.name
    }

    fn display_name(&self) -> &'static str {
        self.display
    }

    fn is_cli(&self, p: &AgentProc) -> bool {
        self.bins.contains(&p.name.as_str())
            && !p.argv.get(1).is_some_and(|a| NOT_TUI.contains(&a.as_str()))
    }

    fn sessions(&self, p: &AgentProc, _procs: &[Proc]) -> Vec<AgentSession> {
        let (Some(db), Some(cwd)) = ((self.db)(), p.cwd.as_deref()) else {
            return Vec::new();
        };
        if !db.exists() {
            return Vec::new();
        }
        // A little slack: the process may report a start a few ms after the
        // session row it immediately touches.
        let since = p.started.map(|s| (s - 5) * 1000);
        sessions_in(&db, cwd, since)
            .into_iter()
            .map(|(id, title)| AgentSession {
                harness: self.name.into(),
                session_id: id,
                cwd: cwd.to_string(),
                name: title,
            })
            .collect()
    }

    fn session_in_argv(&self, argv: &[String]) -> Option<String> {
        super::flag_value(argv, &["--session", "-s"])
    }

    fn resume_command(&self, s: &AgentSession) -> String {
        format!(
            "{} {} {}",
            self.bins[0],
            self.resume_flag,
            crate::rules::shell_quote(&s.session_id)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_recent_top_level_sessions_of_the_directory() {
        let dir = std::env::temp_dir().join(format!("termrevive-sqlite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let sql = "
            create table session (id text primary key, directory text, parent_id text, title text, time_updated integer);
            create table session_v2 (id text primary key, directory text, parent_id text, title text, time_updated integer);
            insert into session values ('old', '/w', null, 'old one', 1000);
            insert into session values ('child', '/w', 'v2a', 'subtask', 9000);
            insert into session_v2 values ('v2a', '/w', null, 'fix it''s bug', 5000);
            insert into session_v2 values ('v2b', '/w', null, '', 7000);
            insert into session_v2 values ('elsewhere', '/x', null, 'x', 8000);";
        let ok = Command::new("/usr/bin/sqlite3")
            .arg(&db)
            .arg(sql)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(
            sessions_in(&db, "/w", Some(2000)),
            vec![
                ("v2b".to_string(), None),
                ("v2a".to_string(), Some("fix it's bug".to_string()))
            ]
        );
        assert_eq!(sessions_in(&db, "/w", None).len(), 3);
        assert!(sessions_in(&db, "/w' or '1'='1", None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_commands_and_tui_detection() {
        let h = kilo();
        let p = |argv: &[&str]| AgentProc {
            pid: 1,
            tty: "ttys001".into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            name: crate::procs::basename(argv[0]).to_string(),
            cwd: None,
            started: None,
        };
        assert!(h.is_cli(&p(&[
            "/opt/homebrew/Cellar/kilo/7.8.17/libexec/kilo",
            "--prompt",
            "x"
        ])));
        assert!(!opencode().is_cli(&p(&["opencode", "serve", "--service"])));
        let s = AgentSession {
            harness: "kilo".into(),
            session_id: "ses_1".into(),
            cwd: "/w".into(),
            name: None,
        };
        assert_eq!(h.resume_command(&s), "kilo --session ses_1");
    }
}
