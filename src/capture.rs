//! Snapshots the running terminals: each backend's layout, paired with the
//! shells of the process table and the live Claude Code sessions.

use crate::backend::{Backend, short_tty};
use crate::claude;
use crate::matcher::{self, Candidate};
use crate::model::{ClaudeSession, Program, SNAPSHOT_VERSION, Snapshot, Window};
use crate::procs::{self, Proc, TtySession};
use anyhow::Result;
use std::collections::HashMap;

pub struct Capture {
    /// `None` when none of the terminals runs.
    pub snapshot: Option<Snapshot>,
    /// Terminals that run but could not be read (permission, remote control
    /// off…), as (backend name, error).
    pub errors: Vec<(String, String)>,
}

pub fn capture(backends: &[Box<dyn Backend>]) -> Result<Capture> {
    let procs = procs::process_table()?;
    let tty_of: HashMap<i32, &str> = procs.iter().map(|p| (p.pid, p.tty.as_str())).collect();
    let claude = claude::live_sessions(|pid| tty_of.contains_key(&pid));

    let mut windows = Vec::new();
    let mut errors = Vec::new();
    let mut any_running = false;
    for b in backends.iter().filter(|b| b.running(&procs)) {
        any_running = true;
        match capture_one(b.as_ref(), &procs, &tty_of, &claude) {
            Ok(ws) => windows.extend(ws),
            Err(e) => errors.push((b.name().to_string(), format!("{}: {e:#}", b.display_name()))),
        }
    }
    let snapshot = any_running.then(|| Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: chrono::Local::now(),
        boot_time: procs::boot_time(),
        backend: String::new(),
        windows,
    });
    Ok(Capture { snapshot, errors })
}

fn capture_one(
    b: &dyn Backend,
    procs: &[Proc],
    tty_of: &HashMap<i32, &str>,
    claude: &HashMap<i32, ClaudeSession>,
) -> Result<Vec<Window>> {
    let layout = b.read_layout(procs)?;
    let reported: Vec<String> = layout
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .flat_map(|t| &t.terminals)
        .filter_map(|t| t.tty.as_deref().and_then(short_tty))
        .collect();
    // Exact when the terminal names its ttys, else every shell it hosts.
    let sessions: Vec<TtySession> = if reported.is_empty() {
        procs::tty_sessions(procs, &b.host_pids(procs))
    } else {
        reported
            .iter()
            .filter_map(|tty| procs::session_on_tty(procs, tty))
            .collect()
    };
    let cwds = procs::cwds(&sessions.iter().map(|s| s.shell_pid).collect::<Vec<_>>());

    let candidates = sessions
        .iter()
        .map(|s| Candidate {
            tty: Some(s.tty.clone()),
            cwd: cwds.get(&s.shell_pid).cloned(),
            program: program_on(s, tty_of, claude),
        })
        .collect();
    let mut layout = layout;
    for t in layout
        .windows
        .iter_mut()
        .flat_map(|w| &mut w.tabs)
        .flat_map(|t| &mut t.terminals)
    {
        t.tty = t.tty.as_deref().and_then(short_tty);
    }
    let mut windows = matcher::assemble(&layout, candidates);
    for w in &mut windows {
        w.backend = b.name().to_string();
    }
    Ok(windows)
}

/// What runs on the shell's tty: a live Claude Code session first (whatever
/// wraps it, e.g. `caffeinate`), else the foreground job.
fn program_on(
    s: &TtySession,
    tty_of: &HashMap<i32, &str>,
    claude: &HashMap<i32, ClaudeSession>,
) -> Option<Program> {
    let on_tty = claude
        .iter()
        .filter(|(pid, _)| tty_of.get(pid) == Some(&s.tty.as_str()))
        .min_by_key(|(pid, _)| **pid);
    match on_tty {
        Some((pid, session)) => Some(Program {
            name: "claude".into(),
            argv: procs::argv(*pid).unwrap_or_else(|| vec!["claude".into()]),
            claude: Some(session.clone()),
        }),
        None => s.fg.as_ref().map(|fg| {
            let argv = procs::argv(fg.pid).unwrap_or_else(|| vec![fg.comm.clone()]);
            Program {
                name: procs::basename(&argv[0]).to_string(),
                argv,
                claude: None,
            }
        }),
    }
}
