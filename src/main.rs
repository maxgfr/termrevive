mod backend;
mod capture;
mod claude;
mod launchd;
mod matcher;
mod model;
mod procs;
mod rules;
mod store;

use anyhow::{Context, Result, bail};
use backend::{Backend, Caps, TabPlan, WindowPlan};
use clap::{Parser, Subcommand};
use model::{Snapshot, Window};
use rules::Config;
use std::path::PathBuf;
use std::time::Duration;

/// Save and restore terminal sessions: windows, tabs, splits, working
/// directories and the programs running in them (Claude Code included), for
/// Ghostty, iTerm2, kitty and Terminal.app.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Terminal to act on: `auto` (every supported one), or ghostty, iterm2,
    /// kitty, terminal.
    #[arg(long, global = true, default_value = "auto")]
    backend: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Snapshot the windows, tabs and panes of the running terminals.
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
        /// Print what would run instead of running it.
        #[arg(long)]
        dry_run: bool,
        /// Restore even if a terminal already has several panes open.
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
    /// Install the LaunchAgent: one process started at login that restores, then
    /// saves every `--interval` seconds (both when no flag is given). With
    /// auto-restore, also sets the terminal settings a restore needs (see
    /// `doctor --fix`).
    Install {
        #[arg(long)]
        auto_save: bool,
        #[arg(long)]
        auto_restore: bool,
        /// Seconds between two automatic saves.
        #[arg(long, default_value_t = 120)]
        interval: u64,
    },
    /// Remove the LaunchAgent.
    Uninstall,
    /// What the LaunchAgent runs: restore once (with --restore), then save
    /// every --save-every seconds until stopped.
    Agent {
        #[arg(long)]
        restore: bool,
        #[arg(long)]
        save_every: Option<u64>,
    },
    /// Print the config file path; --init writes a commented default one.
    Config {
        #[arg(long)]
        init: bool,
    },
    /// Check the terminals (permissions, settings), the agents and the config.
    Doctor {
        /// Set the terminal settings restores need (Ghostty window-save-state,
        /// kitty remote control, iTerm2/Terminal.app window restoration).
        #[arg(long)]
        fix: bool,
    },
    /// Print the agent skill (also installable with `npx skills add maxgfr/termrevive`).
    Skill,
}

const SKILL: &str = include_str!("../skills/termrevive/SKILL.md");

macro_rules! log {
    ($($arg:tt)*) => {
        println!("[{}] {}", chrono::Local::now().format("%F %T"), format!($($arg)*))
    };
}

fn main() {
    // Exit quietly when the reader goes away (`termrevive show | head`).
    // SAFETY: restoring the default SIGPIPE disposition has no preconditions.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("termrevive: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let backends = backend::select(&cli.backend)?;
    match cli.cmd {
        Cmd::Save { auto } => save(&backends, auto),
        Cmd::Restore {
            snapshot,
            dry_run,
            force,
            auto,
        } => restore(&cli.backend, snapshot, dry_run, force, auto),
        Cmd::Show { snapshot } => show(&load(snapshot)?, &Config::load()?),
        Cmd::List => list(),
        Cmd::Install {
            auto_save,
            auto_restore,
            interval,
        } => {
            let both = !auto_save && !auto_restore;
            let config = launchd::AgentConfig {
                restore: auto_restore || both,
                save_every: (auto_save || both).then_some(interval.max(30)),
            };
            if config.restore {
                fix_settings(&backends)?;
            }
            let (path, legacy) = launchd::install(&config)?;
            for p in legacy {
                println!("removed the previous agent {}", p.display());
            }
            println!("installed {} ({})", path.display(), describe(&config));
            println!(
                "log: {}",
                store::state_dir().join("logs/agent.log").display()
            );
            Ok(())
        }
        Cmd::Uninstall => {
            for p in launchd::uninstall()? {
                println!("removed {}", p.display());
            }
            Ok(())
        }
        Cmd::Agent {
            restore,
            save_every,
        } => agent(&cli.backend, &backends, restore, save_every),
        Cmd::Config { init } => config(init),
        Cmd::Doctor { fix } => {
            if fix {
                fix_settings(&backends)?;
            }
            doctor(&backends)
        }
        Cmd::Skill => {
            print!("{SKILL}");
            Ok(())
        }
    }
}

fn describe(c: &launchd::AgentConfig) -> String {
    let mut parts = Vec::new();
    if c.restore {
        parts.push("restore at login".to_string());
    }
    if let Some(n) = c.save_every {
        parts.push(format!("save every {n}s"));
    }
    if parts.is_empty() {
        "does nothing".into()
    } else {
        parts.join(", ")
    }
}

/// The LaunchAgent's process: restore once, then save in a loop. Errors are
/// logged and the loop goes on.
fn agent(
    selection: &str,
    backends: &[Box<dyn Backend>],
    restore_first: bool,
    save_every: Option<u64>,
) -> Result<()> {
    log!("agent started (version {})", env!("CARGO_PKG_VERSION"));
    if restore_first && let Err(e) = restore(selection, None, false, false, true) {
        log!("restore failed: {e:#}");
    }
    let Some(every) = save_every else {
        return Ok(());
    };
    loop {
        if let Err(e) = save(backends, true) {
            log!("save failed: {e:#}");
        }
        std::thread::sleep(Duration::from_secs(every.max(30)));
    }
}

fn summary(s: &Snapshot) -> String {
    let terminals: Vec<String> = s.backends().iter().map(|b| display(b)).collect();
    format!(
        "{} window(s), {} tab(s), {} pane(s), {} Claude session(s){}",
        s.windows.len(),
        s.tab_count(),
        s.pane_count(),
        s.claude_count(),
        if terminals.is_empty() {
            String::new()
        } else {
            format!(" [{}]", terminals.join(", "))
        }
    )
}

/// A backend's display name, or the raw name if it is unknown.
fn display(name: &str) -> String {
    backend::by_name(name)
        .map(|b| b.display_name().to_string())
        .unwrap_or_else(|_| name.to_string())
}

/// Equal up to the status glyphs programs put in titles (Claude Code's spinner
/// changes them every few seconds), so auto-save does not rewrite on every tick.
fn same_layout(a: &[Window], b: &[Window]) -> bool {
    let norm = |ws: &[Window]| {
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

fn save(backends: &[Box<dyn Backend>], auto: bool) -> Result<()> {
    let note = |msg: String| {
        if auto {
            log!("{msg}")
        } else {
            eprintln!("{msg}")
        }
    };
    if auto && store::RestoreLock::held() {
        note("skip: a restore is running".into());
        return Ok(());
    }
    let cap = capture::capture(backends)?;
    for (_, e) in &cap.errors {
        note(format!("warning: {e}"));
    }
    let Some(mut snap) = cap.snapshot else {
        if auto {
            log!("skip: no supported terminal is running");
            return Ok(());
        }
        bail!("no supported terminal is running ({})", backend::names());
    };
    let latest = store::latest().ok().flatten();
    // A terminal that runs but could not be read keeps its windows from the
    // previous snapshot rather than vanishing from this one.
    if let Some(l) = &latest {
        for (name, _) in &cap.errors {
            snap.windows
                .extend(l.windows.iter().filter(|w| &w.backend == name).cloned());
        }
    }
    if snap.pane_count() == 0 {
        if auto {
            log!("skip: no open terminal");
            return Ok(());
        }
        bail!("no open terminal to save");
    }
    if auto {
        // Right after a reboot the terminals may only show their own fresh
        // windows: never let that overwrite the snapshot still to be restored.
        let pending = latest.as_ref().is_some_and(|l| {
            launchd::restores_at_login()
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

/// Restore plans for `windows`. A terminal without splits gets each extra pane
/// as a tab of its own, so nothing is dropped.
fn plan(windows: &[&Window], cfg: &Config, caps: Caps) -> Vec<WindowPlan> {
    windows
        .iter()
        .map(|w| WindowPlan {
            tabs: w
                .tabs
                .iter()
                .filter(|t| !t.panes.is_empty())
                .flat_map(|t| {
                    let panes: Vec<_> = t.panes.iter().map(|p| cfg.launch(p)).collect();
                    if caps.splits {
                        vec![TabPlan {
                            selected: t.selected,
                            panes,
                        }]
                    } else {
                        panes
                            .into_iter()
                            .enumerate()
                            .map(|(i, p)| TabPlan {
                                selected: t.selected && i == 0,
                                panes: vec![p],
                            })
                            .collect()
                    }
                })
                .collect(),
        })
        .collect()
}

/// Panes currently open in `b`.
fn open_panes(b: &dyn Backend) -> Result<usize> {
    Ok(b.read_layout(&procs::process_table()?)?
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| t.terminals.len())
        .sum())
}

fn restore(
    selection: &str,
    path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    auto: bool,
) -> Result<()> {
    let cfg = Config::load()?;
    let snap = load(path)?;
    let names: Vec<String> = snap
        .backends()
        .into_iter()
        .filter(|n| selection == "auto" || n == selection)
        .collect();
    if names.is_empty() {
        bail!("the snapshot has no window for `{selection}`");
    }
    let mut groups: Vec<(Box<dyn Backend>, Vec<WindowPlan>)> = Vec::new();
    for name in &names {
        let b = match backend::by_name(name) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping windows of {name}: {e:#}");
                continue;
            }
        };
        let windows: Vec<&Window> = snap.windows.iter().filter(|w| &w.backend == name).collect();
        // The front window comes first in the snapshot: create it last.
        let plans: Vec<WindowPlan> = plan(&windows, &cfg, b.caps()).into_iter().rev().collect();
        groups.push((b, plans));
    }
    if dry_run {
        for (b, plans) in &groups {
            println!("# {}", b.display_name());
            for p in plans {
                println!("{}", b.restore_script(p, cfg.restore.delay_ms));
            }
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

    // Check every terminal before touching any.
    let procs = procs::process_table()?;
    let mut was_running = Vec::new();
    for (b, _) in &groups {
        let running = b.running(&procs);
        if running && !force && !auto {
            let open = open_panes(b.as_ref())?;
            if open > 1 {
                bail!(
                    "{} already has {open} panes open; restoring would duplicate them. Close \
                     them, pass --force, or restore another terminal with --backend (see \
                     `termrevive doctor` about the terminal's own window restoration).",
                    b.display_name()
                );
            }
        }
        was_running.push(running);
    }

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
    let mut failures = Vec::new();
    for ((b, plans), was_running) in groups.iter().zip(was_running) {
        let result = (|| -> Result<()> {
            b.ensure_running()?;
            let blank = if was_running {
                Vec::new()
            } else {
                // Give the freshly launched app a moment to open its default window.
                std::thread::sleep(Duration::from_secs(1));
                b.blank_windows().unwrap_or_default()
            };
            for (i, p) in plans.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(Duration::from_millis(cfg.restore.delay_ms));
                }
                b.restore_window(p, cfg.restore.delay_ms)
                    .with_context(|| format!("restoring window {}/{}", i + 1, plans.len()))?;
                say(format!(
                    "{}: window {}/{} done ({} tab(s))",
                    b.display_name(),
                    i + 1,
                    plans.len(),
                    p.tabs.len()
                ));
            }
            b.close_windows(&blank)
        })();
        if let Err(e) = result {
            say(format!("{}: {e:#}", b.display_name()));
            failures.push(b.display_name());
        }
    }
    if auto {
        store::mark_restored(boot)?;
    }
    if !failures.is_empty() {
        bail!("could not restore {}", failures.join(", "));
    }
    Ok(())
}

fn show(snap: &Snapshot, cfg: &Config) -> Result<()> {
    println!("{} — {}", snap.taken_at.format("%F %T"), summary(snap));
    for (wi, w) in snap.windows.iter().enumerate() {
        let tag = if w.orphans {
            " (programs without a visible terminal)"
        } else {
            ""
        };
        println!("\nwindow {} · {}{tag}", wi + 1, display(&w.backend));
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

/// Writes every setting restores depend on, for the installed terminals.
fn fix_settings(backends: &[Box<dyn Backend>]) -> Result<()> {
    for b in backends.iter().filter(|b| b.installed()) {
        for s in b.settings().into_iter().filter(|s| !s.ok) {
            let path = b.apply_setting(&s)?;
            println!(
                "set {} {} = {} in {} (was {})",
                b.display_name(),
                s.name,
                s.wanted,
                path.display(),
                s.current.as_deref().unwrap_or("default")
            );
        }
    }
    Ok(())
}

fn doctor(backends: &[Box<dyn Backend>]) -> Result<()> {
    let ok = |b: bool| if b { "ok  " } else { "warn" };
    let procs = procs::process_table()?;
    for b in backends {
        let name = b.display_name();
        if !b.installed() {
            println!("[ -- ] {name}: not installed");
            continue;
        }
        if b.running(&procs) {
            match b.read_layout(&procs) {
                Ok(l) => println!(
                    "[ok  ] {name}: running, readable ({} window(s))",
                    l.windows.len()
                ),
                Err(e) => println!("[warn] {name}: running but unreadable: {e:#}"),
            }
        } else {
            println!("[ok  ] {name}: installed, not running");
        }
        for s in b.settings() {
            let fix = if s.ok {
                String::new()
            } else {
                format!("  → {}; fix: `termrevive doctor --fix`", s.why)
            };
            println!(
                "[{}] {name} {} = {}{fix}",
                ok(s.ok),
                s.name,
                s.current.as_deref().unwrap_or("default"),
            );
        }
    }
    match launchd::agent_config() {
        Some(c) => println!("[ok  ] agent {}: {}", launchd::AGENT_LABEL, describe(&c)),
        None => println!("[ok  ] agent: not installed"),
    }
    let legacy = launchd::legacy_installed();
    if !legacy.is_empty() {
        println!(
            "[warn] previous agents still installed ({}): they relaunch every 2 minutes; \
             run `termrevive install` to replace them with the single agent",
            legacy.join(", ")
        );
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
    use model::{Pane, Tab};

    fn win(title: &str, cwd: &str) -> Vec<Window> {
        vec![Window {
            backend: "ghostty".into(),
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

    #[test]
    fn terminals_without_splits_get_extra_panes_as_tabs() {
        let mut w = win("t", "/").remove(0);
        let pane = w.tabs[0].panes[0].clone();
        w.tabs[0].panes.push(pane);
        let cfg = Config::default();
        let split = plan(&[&w], &cfg, Caps { splits: true });
        assert_eq!(split[0].tabs.len(), 1);
        assert_eq!(split[0].tabs[0].panes.len(), 2);
        let flat = plan(&[&w], &cfg, Caps { splits: false });
        assert_eq!(flat[0].tabs.len(), 2);
        assert!(flat[0].tabs[0].selected && !flat[0].tabs[1].selected);
    }
}
