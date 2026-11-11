//! Snapshots the running terminals: each backend's layout, paired with the
//! shells of the process table and with the agent sessions (Claude Code,
//! Codex…) their harnesses recognize.

use crate::backend::{Backend, short_tty};
use crate::harness::{self, AgentProc, Harness};
use crate::matcher::{self, Candidate};
use crate::model::{AgentSession, Boot, Program, SNAPSHOT_VERSION, Snapshot, Window};
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

/// Snapshots the running terminals among `backends`, during the boot `boot`.
pub fn capture(backends: &[Box<dyn Backend>], boot: &Boot) -> Result<Capture> {
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
        boot_time: boot.time,
        boot_session: boot.session.clone(),
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

/// The `mosh` command line a `mosh-client` was started from. `mosh` connects,
/// then replaces itself with `mosh-client "-# <its arguments> |" <ip> <port>`:
/// rerunning that would need the session key, while `mosh <arguments>`
/// connects again. The arguments were joined with spaces, so one that held a
/// space comes back split.
fn mosh_command(argv: &[String]) -> Option<Vec<String>> {
    if procs::basename(argv.first()?) != "mosh-client" {
        return None;
    }
    let args = argv.get(1)?.strip_prefix("-#")?;
    let args = args.strip_suffix('|').unwrap_or(args);
    let mut out = vec!["mosh".to_string()];
    out.extend(args.split_whitespace().map(String::from));
    (out.len() > 1).then_some(out)
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
                let argv = mosh_command(&argv).unwrap_or(argv);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pane;
    use crate::rules::Config;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn a_mosh_client_comes_back_as_the_mosh_it_was_started_from() {
        let client = args(&[
            "mosh-client",
            "-# --ssh=ssh -p 2222 me@host |",
            "10.0.0.2",
            "60001",
        ]);
        assert_eq!(
            mosh_command(&client),
            Some(args(&["mosh", "--ssh=ssh", "-p", "2222", "me@host"]))
        );
        assert_eq!(mosh_command(&args(&["nvim", "-# x |"])), None);
        assert_eq!(
            mosh_command(&args(&["/usr/local/bin/mosh-client", "-# |", "ip", "1"])),
            None
        );

        // The built-in rule then reruns it, or only types it when it runs a
        // remote command.
        let launch = |argv: Vec<String>| {
            Config::default().launch(&Pane {
                title: String::new(),
                cwd: "/".into(),
                program: Some(Program {
                    name: procs::basename(&argv[0]).to_string(),
                    argv,
                    ..Default::default()
                }),
            })
        };
        let l = launch(mosh_command(&client).unwrap());
        assert_eq!(
            (l.input.as_deref(), l.run),
            (Some("mosh --ssh=ssh -p 2222 me@host"), true)
        );
        let top = args(&["mosh-client", "-# host -- top |", "ip", "1"]);
        assert!(!launch(mosh_command(&top).unwrap()).run);
    }
}
