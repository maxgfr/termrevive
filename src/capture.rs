//! Snapshots the running terminals: each backend's layout, paired with the
//! shells of the process table and with the agent sessions (Claude Code,
//! Codex…) their harnesses recognize.

use crate::backend::{Backend, short_tty};
use crate::harness::{self, AgentProc, Harness};
use crate::matcher::{self, Candidate};
use crate::model::{AgentSession, Program, SNAPSHOT_VERSION, Snapshot, Window};
use crate::procs::{self, Proc, TtySession};
use anyhow::Result;
use std::collections::HashSet;

pub struct Capture {
    /// `None` when none of the terminals runs.
    pub snapshot: Option<Snapshot>,
    /// Terminals that run but could not be read (permission, remote control
    /// off…), as (backend name, error).
    pub errors: Vec<(String, String)>,
}

pub fn capture(backends: &[Box<dyn Backend>]) -> Result<Capture> {
    let procs = procs::process_table()?;
    let harnesses = harness::registry();
    // A session goes to one pane only, even when several CLIs could claim it.
    let mut claimed = HashSet::new();

    let mut windows = Vec::new();
    let mut errors = Vec::new();
    let mut any_running = false;
    for b in backends.iter().filter(|b| b.running(&procs)) {
        any_running = true;
        match capture_one(b.as_ref(), &procs, &harnesses, &mut claimed) {
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
    harnesses: &[Box<dyn Harness>],
    claimed: &mut HashSet<(String, String)>,
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
    let programs = programs(&sessions, procs, harnesses, claimed);

    let candidates = sessions
        .iter()
        .zip(programs)
        .map(|(s, program)| Candidate {
            tty: Some(s.tty.clone()),
            cwd: cwds.get(&s.shell_pid).cloned(),
            program,
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

/// The agent CLI a harness recognizes on the session's tty (whatever wraps
/// it, e.g. `caffeinate`), lowest pid first.
fn agent_on(
    s: &TtySession,
    procs: &[Proc],
    harnesses: &[Box<dyn Harness>],
) -> Option<(usize, AgentProc)> {
    let mut on_tty: Vec<&Proc> = procs
        .iter()
        .filter(|p| p.tty == s.tty && p.pid != s.shell_pid)
        .collect();
    on_tty.sort_by_key(|p| p.pid);
    on_tty.into_iter().find_map(|p| {
        let a = AgentProc::new(p, None);
        harnesses
            .iter()
            .position(|h| h.is_cli(&a))
            .map(|hi| (hi, a))
    })
}

/// What runs on each shell's tty: an agent CLI with its session when a
/// harness recognizes one, else the foreground job.
fn programs(
    sessions: &[TtySession],
    procs: &[Proc],
    harnesses: &[Box<dyn Harness>],
    claimed: &mut HashSet<(String, String)>,
) -> Vec<Option<Program>> {
    let mut agents: Vec<Option<(usize, AgentProc)>> = sessions
        .iter()
        .map(|s| agent_on(s, procs, harnesses))
        .collect();
    let pids: Vec<i32> = agents.iter().flatten().map(|(_, a)| a.pid).collect();
    let cwds = procs::cwds(&pids);
    for (_, a) in agents.iter_mut().flatten() {
        a.cwd = cwds.get(&a.pid).cloned();
    }
    // Earlier processes pick first: with several CLIs in one directory, each
    // gets the session that started closest to it.
    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by_key(|&i| {
        agents[i]
            .as_ref()
            .and_then(|(_, a)| a.started)
            .unwrap_or(i64::MAX)
    });
    let mut out: Vec<Option<Program>> = vec![None; sessions.len()];
    for i in order {
        out[i] = match agents[i].take() {
            Some((hi, a)) => {
                let h = &harnesses[hi];
                let agent = h
                    .sessions(&a, procs)
                    .into_iter()
                    .chain(h.session_in_argv(&a.argv).map(|id| AgentSession {
                        harness: h.name().to_string(),
                        session_id: id,
                        cwd: a.cwd.clone().unwrap_or_default(),
                        name: None,
                    }))
                    .find(|s| claimed.insert((s.harness.clone(), s.session_id.clone())));
                let mut argv = a.argv;
                if let Some(first) = argv.first_mut() {
                    *first = h.command().to_string();
                }
                Some(Program {
                    name: h.name().to_string(),
                    argv,
                    agent,
                    claude: None,
                })
            }
            None => sessions[i].fg.as_ref().map(|fg| {
                let argv = procs::argv(fg.pid).unwrap_or_else(|| vec![fg.comm.clone()]);
                Program {
                    name: procs::basename(&argv[0]).to_string(),
                    argv,
                    agent: None,
                    claude: None,
                }
            }),
        };
    }
    out
}
