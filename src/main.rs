mod backend;
mod claude;
mod launchd;
mod matcher;
mod model;
mod procs;
mod rules;
mod store;

use anyhow::{Context, Result, bail};
use backend::{Backend, TabPlan, WindowPlan};
use clap::{Parser, Subcommand};
use matcher::Candidate;
use model::{Program, SNAPSHOT_VERSION, Snapshot};
use rules::Config;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Save and restore terminal sessions: windows, tabs, splits, working
/// directories and the programs running in them (Claude Code included).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Terminal emulator to talk to.
    #[arg(long, global = true, default_value = "ghostty")]
    backend: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Snapshot the current windows, tabs and panes.
    Save {
        /// Unattended mode (LaunchAgent): skip quietly when there is nothing
        /// sensible to save, or when nothing changed.
        #[arg(long)]
        auto: bool,
    },
    /// Recreate the windows of a snapshot and relaunch their programs.
    Restore {
        /// Snapshot file to use instead of the latest one.
        #[arg(long)]
        snapshot: Option<PathBuf>,
        /// Print the generated scripts instead of running them.
        #[arg(long)]
        dry_run: bool,
        /// Restore even if the terminal already has several panes open.
        #[arg(long)]
        force: bool,
        /// Unattended mode (LaunchAgent at login): only once per boot, and
        /// only from a snapshot taken during a previous boot.
        #[arg(long)]
        auto: bool,
    },
    /// Show what a snapshot contains and what restoring it would run.
    Show {
        #[arg(long)]
        snapshot: Option<PathBuf>,
    },
    /// List saved snapshots, newest first.
    List,
    /// Install the LaunchAgents (both when no flag is given).
    Install {
        #[arg(long)]
        auto_save: bool,
        #[arg(long)]
        auto_restore: bool,
        /// Seconds between two automatic saves.
        #[arg(long, default_value_t = 120)]
        interval: u64,
    },
    /// Remove the LaunchAgents.
    Uninstall,
    /// Print the config file path; --init writes a commented default one.
    Config {
        #[arg(long)]
        init: bool,
    },
    /// Check permissions, terminal settings and installed agents.
    Doctor,
}

macro_rules! log {
    ($($arg:tt)*) => {
        println!("[{}] {}", chrono::Local::now().format("%F %T"), format!($($arg)*))
    };
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("termrevive: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let backend = backend::by_name(&cli.backend)?;
    match cli.cmd {
        Cmd::Save { auto } => save(backend.as_ref(), auto),
        Cmd::Restore {
            snapshot,
            dry_run,
            force,
            auto,
        } => restore(backend.as_ref(), snapshot, dry_run, force, auto),
        Cmd::Show { snapshot } => show(&load(snapshot)?, &Config::load()?),
        Cmd::List => list(),
        Cmd::Install {
            auto_save,
            auto_restore,
            interval,
        } => {
            let both = !auto_save && !auto_restore;
            for p in launchd::install(auto_save || both, auto_restore || both, interval.max(30))? {
                println!("installed {}", p.display());
            }
            println!("logs: {}", store::state_dir().join("logs").display());
            Ok(())
        }
        Cmd::Uninstall => {
            for p in launchd::uninstall()? {
                println!("removed {}", p.display());
            }
            Ok(())
        }
        Cmd::Config { init } => config(init),
        Cmd::Doctor => doctor(backend.as_ref()),
    }
}

/// Reads the terminal and the process table into a snapshot; `None` when the
/// terminal is not running.
fn capture(backend: &dyn Backend) -> Result<Option<Snapshot>> {
    let procs = procs::process_table()?;
    let hosts = backend.host_pids(&procs);
    if hosts.is_empty() {
        return Ok(None);
    }
    let layout = backend.read_layout()?;
    let sessions = procs::tty_sessions(&procs, &hosts);
    let cwds = procs::cwds(&sessions.iter().map(|s| s.shell_pid).collect::<Vec<_>>());
    let tty_of: HashMap<i32, &str> = procs.iter().map(|p| (p.pid, p.tty.as_str())).collect();
    let claude = claude::live_sessions(|pid| tty_of.contains_key(&pid));

    let candidates = sessions
        .iter()
        .map(|s| {
            let on_tty = claude
                .iter()
                .filter(|(pid, _)| tty_of.get(pid) == Some(&s.tty.as_str()))
                .min_by_key(|(pid, _)| **pid);
            let program = match on_tty {
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
            };
            Candidate {
                cwd: cwds.get(&s.shell_pid).cloned(),
                program,
            }
        })
        .collect();

    Ok(Some(Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: chrono::Local::now(),
        boot_time: procs::boot_time(),
        backend: backend.name().into(),
        windows: matcher::assemble(&layout, candidates),
    }))
}

fn summary(s: &Snapshot) -> String {
    format!(
        "{} window(s), {} tab(s), {} pane(s), {} Claude session(s)",
        s.windows.len(),
        s.tab_count(),
        s.pane_count(),
        s.claude_count()
    )
}

/// Equal up to the status glyphs programs put in titles (Claude Code's spinner
/// changes them every few seconds), so auto-save does not rewrite on every tick.
fn same_layout(a: &[model::Window], b: &[model::Window]) -> bool {
    let norm = |ws: &[model::Window]| {
        let mut ws = ws.to_vec();
        for t in ws.iter_mut().flat_map(|w| &mut w.tabs) {
            t.title = matcher::strip_title(&t.title).to_string();
            for p in &mut t.panes {
                p.title = matcher::strip_title(&p.title).to_string();
            }
        }
        ws
    };
    norm(a) == norm(b)
}

fn save(backend: &dyn Backend, auto: bool) -> Result<()> {
    if auto && store::RestoreLock::held() {
        log!("skip: a restore is running");
        return Ok(());
    }
    let Some(snap) = capture(backend)? else {
        if auto {
            log!("skip: {} is not running", backend.name());
            return Ok(());
        }
        bail!("{} is not running", backend.name());
    };
    if snap.pane_count() == 0 {
        if auto {
            log!("skip: no open terminal");
            return Ok(());
        }
        bail!("no open terminal to save");
    }
    let latest = store::latest().ok().flatten();
    if auto {
        // Right after a reboot the terminal may only show its own fresh
        // windows: never let that overwrite the snapshot still to be restored.
        let pending = latest.as_ref().is_some_and(|l| {
            launchd::installed(launchd::RESTORE_LABEL)
                && l.boot_time < snap.boot_time
                && store::restored_boot() != Some(snap.boot_time)
        });
        if pending {
            log!("skip: waiting for the auto-restore of this boot");
            return Ok(());
        }
        if latest.as_ref().is_some_and(|l| {
            l.boot_time == snap.boot_time && same_layout(&l.windows, &snap.windows)
        }) {
            return Ok(());
        }
    }
    let path = store::write(&snap)?;
    if auto {
        log!("saved {} → {}", summary(&snap), path.display());
    } else {
        println!("saved {} → {}", summary(&snap), path.display());
    }
    Ok(())
}

fn load(path: Option<PathBuf>) -> Result<Snapshot> {
    match path {
        Some(p) => store::read(&p),
        None => store::latest()?.context("no snapshot yet: run `termrevive save` first"),
    }
}

fn plan(snap: &Snapshot, cfg: &Config) -> Vec<WindowPlan> {
    snap.windows
        .iter()
        .map(|w| WindowPlan {
            tabs: w
                .tabs
                .iter()
                .filter(|t| !t.panes.is_empty())
                .map(|t| TabPlan {
                    selected: t.selected,
                    panes: t.panes.iter().map(|p| cfg.launch(p)).collect(),
                })
                .collect(),
        })
        .collect()
}

fn restore(
    backend: &dyn Backend,
    path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    auto: bool,
) -> Result<()> {
    let cfg = Config::load()?;
    let snap = load(path)?;
    if snap.backend != backend.name() {
        bail!(
            "snapshot was taken with `{}`, not `{}`",
            snap.backend,
            backend.name()
        );
    }
    // The front window comes first in the snapshot: create it last.
    let plans: Vec<WindowPlan> = plan(&snap, &cfg).into_iter().rev().collect();
    if dry_run {
        for p in &plans {
            println!("{}", backend.restore_script(p, cfg.restore.delay_ms));
        }
        return Ok(());
    }

    let boot = procs::boot_time();
    if auto {
        if snap.boot_time >= boot {
            log!("skip: latest snapshot is from this boot");
            return Ok(());
        }
        if store::restored_boot() == Some(boot) {
            log!("skip: already restored for this boot");
            return Ok(());
        }
    }

    let was_running = !backend.host_pids(&procs::process_table()?).is_empty();
    backend.ensure_running()?;
    let blank = if was_running {
        if !force && !auto {
            let open: usize = backend
                .read_layout()?
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .map(|t| t.terminals.len())
                .sum();
            if open > 1 {
                bail!(
                    "{} already has {open} panes open; restoring would duplicate them. \
                     Close them or pass --force (and see `termrevive doctor` about the \
                     terminal's own window restoration).",
                    backend.name()
                );
            }
        }
        Vec::new()
    } else {
        // Give the freshly launched app a moment to open its default window.
        std::thread::sleep(Duration::from_secs(1));
        backend.blank_windows().unwrap_or_default()
    };

    let _lock = store::RestoreLock::acquire()?;
    let say = |msg: String| {
        if auto {
            log!("{msg}")
        } else {
            println!("{msg}")
        }
    };
    say(format!(
        "restoring {} from {}",
        summary(&snap),
        snap.taken_at.format("%F %T")
    ));
    for (i, p) in plans.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(cfg.restore.delay_ms));
        }
        backend
            .restore_window(p, cfg.restore.delay_ms)
            .with_context(|| format!("restoring window {}/{}", i + 1, plans.len()))?;
        say(format!(
            "window {}/{} done ({} tab(s))",
            i + 1,
            plans.len(),
            p.tabs.len()
        ));
    }
    backend.close_windows(&blank)?;
    if auto {
        store::mark_restored(boot)?;
    }
    Ok(())
}

fn show(snap: &Snapshot, cfg: &Config) -> Result<()> {
    println!(
        "{} ({}) — {}",
        snap.taken_at.format("%F %T"),
        snap.backend,
        summary(snap)
    );
    for (wi, w) in snap.windows.iter().enumerate() {
        let tag = if w.orphans {
            " (programs without a visible terminal)"
        } else {
            ""
        };
        println!("\nwindow {}{tag}", wi + 1);
        for (ti, t) in w.tabs.iter().enumerate() {
            let sel = if t.selected { "*" } else { " " };
            println!("  {sel}tab {}  {}", ti + 1, t.title);
            for p in &t.panes {
                let l = cfg.launch(p);
                let action = match (&l.input, l.run) {
                    (Some(cmd), true) => format!("run   {cmd}"),
                    (Some(cmd), false) => format!("type  {cmd}"),
                    (None, _) => "shell".into(),
                };
                println!("      {:<6}{}", "", action);
                println!("      {:<6}in {}", "", l.cwd);
            }
        }
    }
    Ok(())
}

fn list() -> Result<()> {
    let paths = store::list()?;
    if paths.is_empty() {
        println!("no snapshot yet");
    }
    for p in paths {
        match store::read(&p) {
            Ok(s) => println!("{}  {}", p.display(), summary(&s)),
            Err(e) => println!("{}  (unreadable: {e})", p.display()),
        }
    }
    Ok(())
}

fn config(init: bool) -> Result<()> {
    let path = rules::config_path().context("no home directory")?;
    if init {
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, rules::DEFAULT_CONFIG)?;
        println!("wrote {}", path.display());
    } else {
        let state = if path.exists() {
            ""
        } else {
            " (not created; built-in defaults apply)"
        };
        println!("{}{state}", path.display());
    }
    Ok(())
}

/// `window-save-state` from Ghostty's config files, last one wins.
fn ghostty_window_save_state() -> Option<String> {
    let home = dirs::home_dir()?;
    let files = [
        ".config/ghostty/config",
        ".config/ghostty/config.ghostty",
        "Library/Application Support/com.mitchellh.ghostty/config",
        "Library/Application Support/com.mitchellh.ghostty/config.ghostty",
    ];
    let mut value = None;
    for f in files {
        let Ok(text) = std::fs::read_to_string(home.join(f)) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "window-save-state"
            {
                value = Some(v.trim().to_string());
            }
        }
    }
    value
}

fn doctor(backend: &dyn Backend) -> Result<()> {
    let ok = |b: bool| if b { "ok  " } else { "warn" };
    let running = !backend.host_pids(&procs::process_table()?).is_empty();
    println!("[{}] {} running", ok(running), backend.name());
    if running {
        match backend.read_layout() {
            Ok(l) => println!(
                "[ok  ] automation access ({} window(s) visible)",
                l.windows.len()
            ),
            Err(e) => println!("[warn] automation access: {e:#}"),
        }
    }
    if backend.name() == "ghostty" {
        let wss = ghostty_window_save_state();
        let never = wss.as_deref() == Some("never");
        println!(
            "[{}] ghostty window-save-state = {}{}",
            ok(never),
            wss.as_deref().unwrap_or("default"),
            if never {
                ""
            } else {
                "  → Ghostty reopens its own (empty) windows at launch, duplicating a restore; \
                 add `window-save-state = never` to ~/.config/ghostty/config"
            }
        );
    }
    for label in [launchd::SAVE_LABEL, launchd::RESTORE_LABEL] {
        let state = if launchd::installed(label) {
            "installed"
        } else {
            "not installed"
        };
        println!("[ok  ] {label}: {state}");
    }
    match store::latest() {
        Ok(Some(s)) => println!(
            "[ok  ] latest snapshot {}: {}",
            s.taken_at.format("%F %T"),
            summary(&s)
        ),
        Ok(None) => println!("[warn] no snapshot yet"),
        Err(e) => println!("[warn] latest snapshot: {e:#}"),
    }
    match rules::config_path() {
        Some(p) if p.exists() => match Config::load() {
            Ok(_) => println!("[ok  ] config {}", p.display()),
            Err(e) => println!("[warn] config: {e:#}"),
        },
        _ => println!("[ok  ] config: built-in defaults"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Pane, Tab, Window};

    fn win(title: &str, cwd: &str) -> Vec<Window> {
        vec![Window {
            orphans: false,
            tabs: vec![Tab {
                title: title.into(),
                selected: true,
                panes: vec![Pane {
                    title: title.into(),
                    cwd: cwd.into(),
                    program: None,
                }],
            }],
        }]
    }

    #[test]
    fn spinner_glyphs_do_not_count_as_changes() {
        assert!(same_layout(
            &win("✳ fix-bug", "/a"),
            &win("◐ fix-bug", "/a")
        ));
        assert!(!same_layout(
            &win("✳ fix-bug", "/a"),
            &win("✳ fix-bug", "/b")
        ));
        assert!(!same_layout(&win("✳ fix-bug", "/a"), &win("✳ other", "/a")));
    }
}
