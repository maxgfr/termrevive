mod backend;
mod capture;
mod exec;
mod harness;
mod launchd;
mod matcher;
mod model;
mod paths;
mod procs;
mod rules;
mod store;

use anyhow::{Context, Result, bail};
use backend::{Backend, Caps, TabPlan, WindowPlan};
use clap::{Parser, Subcommand};
use model::{Boot, Snapshot, Window};
use rules::Config;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use store::Store;

/// Save and restore terminal sessions: windows, tabs, splits, working
/// directories and the programs running in them, for Ghostty, iTerm2, kitty
/// and Terminal.app. Agent CLIs resume their own session: Claude Code, Codex,
/// Mistral Vibe, opencode and Kilo.
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
    /// Recreate the windows of a snapshot and relaunch their programs. By
    /// default, the latest snapshot; after a reboot that was not restored
    /// yet, the last one taken before it.
    Restore {
        /// Snapshot file to use instead.
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
    let cli = Cli::parse();
    // Exit quietly when the reader goes away (`termrevive show | head`). Not
    // the agent: a child that exits before reading its input would end it
    // without a word.
    if !matches!(cli.cmd, Cmd::Agent { .. }) {
        // SAFETY: restoring the default SIGPIPE disposition has no preconditions.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if let Err(e) = run(cli) {
        eprintln!("termrevive: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let backends = backend::select(&cli.backend)?;
    let store = Store::open();
    match cli.cmd {
        Cmd::Save { auto } => save(&backends, auto, &store, &mut Notes::new(auto)),
        Cmd::Restore {
            snapshot,
            dry_run,
            force,
            auto,
        } => restore(
            &RestoreArgs {
                selection: &cli.backend,
                path: snapshot,
                dry_run,
                force,
                auto,
            },
            &store,
        ),
        Cmd::Show { snapshot } => show(&pick(&store, snapshot)?, &Config::load()?),
        Cmd::List => list(&store),
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
            println!("log: {}", store.log_path().display());
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
        } => agent(&cli.backend, &backends, restore, save_every, &store),
        Cmd::Config { init } => config(init),
        Cmd::Doctor { fix } => {
            if fix {
                fix_settings(&backends)?;
            }
            doctor(&backends, &store)
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

/// Skips and warnings. On the command line they go to stderr. The agent logs
/// each one when it appears, not again while it repeats from one save to the
/// next: an unreadable terminal would otherwise log the same lines every tick.
struct Notes {
    auto: bool,
    previous: HashSet<String>,
    current: HashSet<String>,
}

impl Notes {
    fn new(auto: bool) -> Notes {
        Notes {
            auto,
            previous: HashSet::new(),
            current: HashSet::new(),
        }
    }

    /// Whether `msg` is new since the previous tick; records it for the next.
    fn fresh(&mut self, msg: &str) -> bool {
        self.current.insert(msg.to_string());
        !self.previous.contains(msg)
    }

    fn note(&mut self, msg: String) {
        if !self.auto {
            eprintln!("{msg}");
        } else if self.fresh(&msg) {
            log!("{msg}");
        }
    }

    /// Ends a tick: what was noted becomes what repeats.
    fn tick(&mut self) {
        self.previous = std::mem::take(&mut self.current);
    }
}

/// The LaunchAgent's process: restore once, then save in a loop. Errors are
/// logged and the loop goes on.
fn agent(
    selection: &str,
    backends: &[Box<dyn Backend>],
    restore_first: bool,
    save_every: Option<u64>,
    store: &Store,
) -> Result<()> {
    store.rotate_log();
    log!("agent started (version {})", env!("CARGO_PKG_VERSION"));
    if restore_first {
        let args = RestoreArgs {
            selection,
            path: None,
            dry_run: false,
            force: false,
            auto: true,
        };
        if let Err(e) = restore(&args, store) {
            log!("restore failed: {e:#}");
        }
    }
    let Some(every) = save_every else {
        return Ok(());
    };
    let mut notes = Notes::new(true);
    loop {
        if let Err(e) = save(backends, true, store, &mut notes) {
            notes.note(format!("save failed: {e:#}"));
        }
        notes.tick();
        store.rotate_log();
        std::thread::sleep(Duration::from_secs(every.max(30)));
    }
}

fn summary(s: &Snapshot) -> String {
    let terminals: Vec<String> = s.backends().iter().map(|b| display(b)).collect();
    let counts = s.agent_counts();
    let agents: Vec<String> = counts
        .iter()
        .map(|(h, n)| format!("{n} {}", harness::display(h)))
        .collect();
    format!(
        "{} window(s), {} tab(s), {} pane(s), {} agent session(s){}{}",
        s.windows.len(),
        s.tab_count(),
        s.pane_count(),
        counts.iter().map(|(_, n)| n).sum::<usize>(),
        if agents.is_empty() {
            String::new()
        } else {
            format!(" ({})", agents.join(", "))
        },
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

/// A terminal that runs but could not be read (`unreadable`, by name) keeps
/// its windows from the previous snapshot rather than vanishing from this one.
fn carry_over(snap: &mut Snapshot, latest: Option<&Snapshot>, unreadable: &[(String, String)]) {
    if let Some(l) = latest {
        for (name, _) in unreadable {
            snap.windows
                .extend(l.windows.iter().filter(|w| &w.backend == name).cloned());
        }
    }
}

/// What auto-save does with a capture.
#[derive(Debug, PartialEq)]
enum AutoSave {
    Write,
    /// Same layout as the latest snapshot.
    Unchanged,
    /// After a reboot, before that boot's restore: the terminals may only
    /// show their own fresh windows, which must not replace the snapshot
    /// still to be restored.
    WaitForRestore,
}

fn auto_save(
    latest: Option<&Snapshot>,
    snap: &Snapshot,
    restored: Option<&Boot>,
    restores_at_login: impl FnOnce() -> bool,
) -> AutoSave {
    let Some(latest) = latest else {
        return AutoSave::Write;
    };
    let boot = snap.boot();
    if latest.boot().same(&boot) {
        return if same_layout(&latest.windows, &snap.windows) {
            AutoSave::Unchanged
        } else {
            AutoSave::Write
        };
    }
    // Reading the agent's plist costs a process: only after a reboot.
    if !restored.is_some_and(|r| r.same(&boot)) && restores_at_login() {
        AutoSave::WaitForRestore
    } else {
        AutoSave::Write
    }
}

fn save(backends: &[Box<dyn Backend>], auto: bool, store: &Store, notes: &mut Notes) -> Result<()> {
    if auto && store.restoring() {
        notes.note("skip: a restore is running".into());
        return Ok(());
    }
    let cap = capture::capture(backends, &procs::boot())?;
    for (_, e) in &cap.errors {
        notes.note(format!("warning: {e}"));
    }
    let Some(mut snap) = cap.snapshot else {
        if auto {
            notes.note("skip: no supported terminal is running".into());
            return Ok(());
        }
        bail!("no supported terminal is running ({})", backend::names());
    };
    let latest = store.latest().ok().flatten();
    carry_over(&mut snap, latest.as_ref(), &cap.errors);
    if snap.pane_count() == 0 {
        if auto {
            notes.note("skip: no open terminal".into());
            return Ok(());
        }
        bail!("no open terminal to save");
    }
    if auto {
        match auto_save(
            latest.as_ref(),
            &snap,
            store.restored_boot().as_ref(),
            launchd::restores_at_login,
        ) {
            AutoSave::Write => {}
            AutoSave::Unchanged => return Ok(()),
            AutoSave::WaitForRestore => {
                notes.note("skip: waiting for the auto-restore of this boot".into());
                return Ok(());
            }
        }
    }
    let path = store.write(&snap)?;
    if auto {
        log!("saved {} → {}", summary(&snap), path.display());
    } else {
        // What is open is what the user wants back: after a reboot, no
        // longer the snapshot from before it.
        store.mark_saved_by_hand(&snap.boot())?;
        println!("saved {} → {}", summary(&snap), path.display());
    }
    Ok(())
}

/// Which snapshot `restore` and `show` use when none is named.
#[derive(Debug, PartialEq)]
enum Pick {
    Latest,
    /// The latest snapshot is from this boot, which nothing restored yet:
    /// saves of the fresh windows replaced the one from before the reboot.
    PreviousBoot,
}

/// How long after a reboot the snapshot from before it stays the default.
const PREVIOUS_BOOT_DEFAULT: Duration = Duration::from_secs(24 * 3600);

/// What decides between the latest snapshot and the previous boot's.
struct Since<'a> {
    boot: &'a Boot,
    /// The boot restored last.
    restored: Option<&'a Boot>,
    /// The boot during which the user last saved by hand.
    saved_by_hand: Option<&'a Boot>,
    /// How long ago the previous boot's snapshot was kept.
    kept_for: Option<Duration>,
}

/// The previous boot's snapshot is the default only right after a reboot:
/// until this boot is restored, or the user saves what is open by hand, and
/// for a day at most.
fn default_snapshot(latest: &Snapshot, previous: Option<&Snapshot>, s: &Since) -> Pick {
    let this_boot = |b: Option<&Boot>| b.is_some_and(|b| b.same(s.boot));
    let unsettled = latest.boot().same(s.boot)
        && !this_boot(s.restored)
        && !this_boot(s.saved_by_hand)
        && s.kept_for.is_some_and(|d| d < PREVIOUS_BOOT_DEFAULT);
    if unsettled && previous.is_some_and(|p| !p.boot().same(s.boot)) {
        Pick::PreviousBoot
    } else {
        Pick::Latest
    }
}

/// The snapshot at `path`, or the default one (see [`Pick`]).
fn pick(store: &Store, path: Option<PathBuf>) -> Result<Snapshot> {
    if let Some(p) = path {
        return store::read(&p);
    }
    let latest = store
        .latest()?
        .context("no snapshot yet: run `termrevive save` first")?;
    let previous = store.previous_boot().ok().flatten();
    let boot = procs::boot();
    let (restored, by_hand) = (store.restored_boot(), store.saved_by_hand());
    let since = Since {
        boot: &boot,
        restored: restored.as_ref(),
        saved_by_hand: by_hand.as_ref(),
        kept_for: store.previous_boot_age(),
    };
    match default_snapshot(&latest, previous.as_ref(), &since) {
        Pick::Latest => Ok(latest),
        Pick::PreviousBoot => {
            let previous = previous.expect("checked by default_snapshot");
            eprintln!(
                "note: this boot was not restored yet, so this is the last snapshot from before \
                 it ({}); `--snapshot {}` uses the latest",
                previous.taken_at.format("%F %T"),
                store.latest_path().display()
            );
            Ok(previous)
        }
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

/// Whether restoring the terminals `names` brings back all of `snap`.
fn covers(snap: &Snapshot, names: &[String]) -> bool {
    snap.backends().iter().all(|b| names.contains(b))
}

/// Why the restore at login has nothing to do, if it has not.
fn auto_restore_skip(
    snap: &Snapshot,
    boot: &Boot,
    restored: Option<&Boot>,
) -> Option<&'static str> {
    if snap.boot().same(boot) {
        Some("latest snapshot is from this boot")
    } else if restored.is_some_and(|r| r.same(boot)) {
        Some("already restored for this boot")
    } else {
        None
    }
}

/// Panes currently open in `b`.
fn open_panes(b: &dyn Backend, procs: &[procs::Proc]) -> Result<usize> {
    Ok(b.read_layout(procs)?
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| t.terminals.len())
        .sum())
}

struct RestoreArgs<'a> {
    selection: &'a str,
    path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    auto: bool,
}

fn restore(args: &RestoreArgs, store: &Store) -> Result<()> {
    let cfg = Config::load()?;
    let boot = procs::boot();
    let snap = if args.auto {
        // At login: the latest snapshot or nothing.
        let snap = store
            .latest()?
            .context("no snapshot yet: run `termrevive save` first")?;
        if let Some(why) = auto_restore_skip(&snap, &boot, store.restored_boot().as_ref()) {
            log!("skip: {why}");
            return Ok(());
        }
        snap
    } else {
        pick(store, args.path.clone())?
    };
    let selection = args.selection;
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
    if args.dry_run {
        for (b, plans) in &groups {
            println!("# {}", b.display_name());
            for p in plans {
                println!("{}", b.restore_script(p, cfg.restore.delay_ms));
            }
        }
        return Ok(());
    }

    let procs = procs::process_table()?;
    let say = |msg: String| {
        if args.auto {
            log!("{msg}")
        } else {
            println!("{msg}")
        }
    };
    replay(
        &groups,
        &Replay {
            procs: &procs,
            snap: &snap,
            boot: &boot,
            force: args.force,
            auto: args.auto,
            delay_ms: cfg.restore.delay_ms,
        },
        store,
        &say,
    )
}

/// What a restore runs with.
struct Replay<'a> {
    procs: &'a [procs::Proc],
    snap: &'a Snapshot,
    boot: &'a Boot,
    force: bool,
    auto: bool,
    delay_ms: u64,
}

/// Rebuilds the windows of `groups` (each terminal with its plans), once
/// every terminal was checked, under the restore lock; records the boot as
/// restored when that is what happened.
fn replay(
    groups: &[(Box<dyn Backend>, Vec<WindowPlan>)],
    r: &Replay,
    store: &Store,
    say: &dyn Fn(String),
) -> Result<()> {
    // Check every terminal before touching any.
    let mut was_running = Vec::new();
    for (b, _) in groups {
        let running = b.running(r.procs);
        if running && !r.force && !r.auto {
            let open = open_panes(b.as_ref(), r.procs)?;
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

    let _lock = store.lock()?;
    say(format!(
        "restoring {} from {}",
        summary(r.snap),
        r.snap.taken_at.format("%F %T")
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
                    std::thread::sleep(Duration::from_millis(r.delay_ms));
                }
                b.restore_window(p, r.delay_ms)
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
    // A manual restore of the whole snapshot is this boot's restore too:
    // auto-save may go on, and the agent has nothing left to restore. Not
    // when it failed, or left a terminal out: a retry must find the same
    // snapshot. The one at login runs once, whatever happens.
    let restored: Vec<String> = groups.iter().map(|(b, _)| b.name().to_string()).collect();
    if r.auto || (failures.is_empty() && covers(r.snap, &restored)) {
        store.mark_restored(r.boot)?;
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

fn list(store: &Store) -> Result<()> {
    let line = |p: &std::path::Path| match store::read(p) {
        Ok(s) => println!("{}  {}", p.display(), summary(&s)),
        Err(e) => println!("{}  (unreadable: {e})", p.display()),
    };
    let paths = store.list()?;
    if paths.is_empty() {
        println!("no snapshot yet");
    }
    for p in &paths {
        line(p);
    }
    let previous = store.previous_boot_path();
    if previous.exists() {
        println!("\nlast snapshot of the previous boot:");
        line(&previous);
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

fn doctor(backends: &[Box<dyn Backend>], store: &Store) -> Result<()> {
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
    match store.latest() {
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
            Ok(cfg) => {
                let problems = cfg.problems();
                if problems.is_empty() {
                    println!("[ok  ] config {}", p.display());
                }
                for problem in problems {
                    println!("[warn] config {}: {problem}", p.display());
                }
            }
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

    fn boot(time: i64, session: &str) -> Boot {
        Boot {
            time,
            session: Some(session.into()),
        }
    }

    fn snap(boot: &Boot, windows: Vec<Window>) -> Snapshot {
        Snapshot {
            windows,
            ..store::empty_snapshot(boot)
        }
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
    fn only_a_restore_of_every_terminal_covers_the_snapshot() {
        let mut windows = win("a", "/");
        windows.extend(win("b", "/"));
        windows[1].backend = "iterm2".into();
        let snap = snap(&boot(1, "A"), windows);
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(covers(&snap, &names(&["ghostty", "iterm2"])));
        assert!(!covers(&snap, &names(&["ghostty"])), "--backend ghostty");
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

    #[test]
    fn auto_save_writes_changes_and_waits_for_the_restore_after_a_reboot() {
        let (old, new) = (boot(1, "A"), boot(2, "B"));
        let before = snap(&old, win("a", "/a"));
        let fresh = snap(&new, win("zsh", "/"));
        let never = || -> bool { panic!("the plist is read only after a reboot") };

        assert_eq!(auto_save(None, &fresh, None, never), AutoSave::Write);
        let same = snap(&old, win("◐ a", "/a"));
        assert_eq!(
            auto_save(Some(&before), &same, None, never),
            AutoSave::Unchanged
        );
        let moved = snap(&old, win("a", "/b"));
        assert_eq!(
            auto_save(Some(&before), &moved, None, never),
            AutoSave::Write
        );

        // After a reboot, with auto-restore on: wait until this boot's restore.
        assert_eq!(
            auto_save(Some(&before), &fresh, Some(&old), || true),
            AutoSave::WaitForRestore
        );
        assert_eq!(
            auto_save(Some(&before), &fresh, Some(&new), never),
            AutoSave::Write
        );
        // Auto-save only: written (the store keeps the previous boot's).
        assert_eq!(
            auto_save(Some(&before), &fresh, None, || false),
            AutoSave::Write
        );
    }

    #[test]
    fn unreadable_terminals_keep_their_previous_windows() {
        let mut latest = win("g", "/g");
        latest.extend(win("i", "/i"));
        latest[1].backend = "iterm2".into();
        let latest = snap(&boot(1, "A"), latest);
        let mut now = snap(&boot(1, "A"), win("g2", "/g2"));
        carry_over(
            &mut now,
            Some(&latest),
            &[("iterm2".into(), "refused".into())],
        );
        let titles: Vec<&str> = now
            .windows
            .iter()
            .map(|w| w.tabs[0].title.as_str())
            .collect();
        assert_eq!(titles, ["g2", "i"]);
        carry_over(&mut now, None, &[("iterm2".into(), "refused".into())]);
        assert_eq!(now.windows.len(), 2);
    }

    #[test]
    fn the_restore_at_login_runs_once_per_boot_from_an_earlier_one() {
        let (old, new) = (boot(1, "A"), boot(2, "B"));
        let before = snap(&old, win("a", "/"));
        assert_eq!(auto_restore_skip(&before, &new, None), None);
        assert_eq!(auto_restore_skip(&before, &new, Some(&old)), None);
        assert_eq!(
            auto_restore_skip(&before, &new, Some(&new)),
            Some("already restored for this boot")
        );
        let after = snap(&new, win("a", "/"));
        assert_eq!(
            auto_restore_skip(&after, &new, None),
            Some("latest snapshot is from this boot")
        );
    }

    #[test]
    fn an_unrestored_reboot_defaults_to_the_snapshot_from_before_it() {
        let (old, new) = (boot(1, "A"), boot(2, "B"));
        let before = snap(&old, win("a", "/"));
        let fresh = snap(&new, win("zsh", "/"));
        let hour = Some(Duration::from_secs(3600));
        let since = |restored, saved_by_hand, kept_for| Since {
            boot: &new,
            restored,
            saved_by_hand,
            kept_for,
        };
        // Auto-saves of the fresh windows replaced latest.json.
        let pick = |s: &Since| default_snapshot(&fresh, Some(&before), s);
        assert_eq!(
            pick(&since(Some(&old), Some(&old), hour)),
            Pick::PreviousBoot
        );
        // This boot was restored, or saved by hand: the latest is what the
        // user wants now.
        assert_eq!(pick(&since(Some(&new), None, hour)), Pick::Latest);
        assert_eq!(pick(&since(None, Some(&new), hour)), Pick::Latest);
        // More than a day after the reboot, or no age known.
        let days = Some(Duration::from_secs(2 * 24 * 3600));
        assert_eq!(pick(&since(None, None, days)), Pick::Latest);
        assert_eq!(pick(&since(None, None, None)), Pick::Latest);
        // Nothing saved since the reboot, or nothing kept from before it.
        let s = since(None, None, hour);
        assert_eq!(default_snapshot(&before, Some(&before), &s), Pick::Latest);
        assert_eq!(default_snapshot(&fresh, None, &s), Pick::Latest);
    }

    /// A terminal that records what a restore asks of it.
    struct Fake {
        name: &'static str,
        running: bool,
        open: usize,
        fails: bool,
        calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl Backend for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn display_name(&self) -> &'static str {
            self.name
        }
        fn caps(&self) -> Caps {
            Caps { splits: true }
        }
        fn installed(&self) -> bool {
            true
        }
        fn host_pids(&self, _: &[procs::Proc]) -> Vec<i32> {
            if self.running { vec![1] } else { Vec::new() }
        }
        fn read_layout(&self, _: &[procs::Proc]) -> Result<backend::RawLayout> {
            let terminal = backend::RawTerminal {
                id: String::new(),
                title: String::new(),
                cwd: String::new(),
                tty: None,
            };
            Ok(backend::RawLayout {
                windows: vec![backend::RawWindow {
                    tabs: vec![backend::RawTab {
                        title: String::new(),
                        selected: true,
                        terminals: vec![terminal; self.open],
                    }],
                }],
            })
        }
        fn ensure_running(&self) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("{}: start", self.name));
            Ok(())
        }
        fn blank_windows(&self) -> Result<Vec<String>> {
            Ok(vec!["blank".into()])
        }
        fn close_windows(&self, ids: &[String]) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("{}: close {}", self.name, ids.join(",")));
            Ok(())
        }
        fn restore_script(&self, _: &WindowPlan, _: u64) -> String {
            String::new()
        }
        fn restore_window(&self, w: &WindowPlan, _: u64) -> Result<()> {
            if self.fails {
                bail!("did not answer");
            }
            self.calls.borrow_mut().push(format!(
                "{}: window of {} tab(s)",
                self.name,
                w.tabs.len()
            ));
            Ok(())
        }
    }

    type Calls = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    fn fake(
        name: &'static str,
        running: bool,
        open: usize,
        fails: bool,
        calls: &Calls,
    ) -> Box<dyn Backend> {
        Box::new(Fake {
            name,
            running,
            open,
            fails,
            calls: calls.clone(),
        })
    }

    fn one_window() -> Vec<WindowPlan> {
        vec![WindowPlan {
            tabs: vec![TabPlan {
                selected: true,
                panes: vec![rules::Launch {
                    cwd: "/".into(),
                    input: None,
                    run: false,
                }],
            }],
        }]
    }

    fn replay_with(
        groups: &[(Box<dyn Backend>, Vec<WindowPlan>)],
        snap: &Snapshot,
        store: &Store,
        (force, auto): (bool, bool),
    ) -> Result<()> {
        let boot = boot(2, "B");
        let r = Replay {
            procs: &[],
            snap,
            boot: &boot,
            force,
            auto,
            delay_ms: 0,
        };
        replay(groups, &r, store, &|_| {})
    }

    #[test]
    fn a_restore_refuses_to_duplicate_open_panes_before_touching_anything() {
        let store = store::scratch("replay-refuse");
        let calls = Calls::default();
        let mut windows = win("a", "/");
        windows.extend(win("b", "/"));
        windows[1].backend = "busy".into();
        windows[0].backend = "idle".into();
        let s = snap(&boot(1, "A"), windows);
        let groups = vec![
            (fake("idle", true, 1, false, &calls), one_window()),
            (fake("busy", true, 3, false, &calls), one_window()),
        ];
        let err = replay_with(&groups, &s, &store, (false, false)).unwrap_err();
        assert!(
            err.to_string().contains("busy already has 3 panes open"),
            "{err}"
        );
        assert!(calls.borrow().is_empty(), "nothing was touched");
        assert_eq!(store.restored_boot(), None);

        // --force, or the restore at login, goes ahead.
        replay_with(&groups, &s, &store, (true, false)).unwrap();
        assert_eq!(
            *calls.borrow(),
            [
                "idle: start",
                "idle: window of 1 tab(s)",
                "idle: close ",
                "busy: start",
                "busy: window of 1 tab(s)",
                "busy: close "
            ]
        );
        assert_eq!(
            store.restored_boot(),
            Some(boot(2, "B")),
            "the whole snapshot"
        );
    }

    #[test]
    fn a_failed_or_partial_restore_leaves_the_boot_unrestored() {
        let calls = Calls::default();
        let mut windows = win("a", "/");
        windows.extend(win("b", "/"));
        windows[0].backend = "ok".into();
        windows[1].backend = "broken".into();
        let s = snap(&boot(1, "A"), windows);
        let groups = || {
            vec![
                (fake("ok", true, 0, false, &calls), one_window()),
                (fake("broken", true, 0, true, &calls), one_window()),
            ]
        };

        let store = store::scratch("replay-failed");
        let err = replay_with(&groups(), &s, &store, (false, false)).unwrap_err();
        assert!(
            err.to_string().contains("could not restore broken"),
            "{err}"
        );
        assert_eq!(
            store.restored_boot(),
            None,
            "a retry finds the same snapshot"
        );

        // One terminal of two (--backend): not the whole snapshot.
        let store = store::scratch("replay-partial");
        replay_with(&groups()[..1], &s, &store, (false, false)).unwrap();
        assert_eq!(store.restored_boot(), None);

        // At login it runs once, whatever happens.
        let store = store::scratch("replay-login");
        assert!(replay_with(&groups(), &s, &store, (false, true)).is_err());
        assert_eq!(store.restored_boot(), Some(boot(2, "B")));
    }

    #[test]
    fn a_terminal_started_by_the_restore_loses_its_blank_window() {
        let store = store::scratch("replay-blank");
        let calls = Calls::default();
        let mut w = win("a", "/");
        w[0].backend = "cold".into();
        let s = snap(&boot(1, "A"), w);
        let groups = vec![(fake("cold", false, 0, false, &calls), one_window())];
        replay_with(&groups, &s, &store, (false, false)).unwrap();
        assert_eq!(
            calls.borrow().last().map(String::as_str),
            Some("cold: close blank")
        );
        // The lock is given back.
        assert!(!store.restoring());
    }

    #[test]
    fn the_agent_logs_a_repeated_note_once() {
        let mut notes = Notes::new(true);
        assert!(notes.fresh("warning: x"));
        assert!(
            notes.fresh("warning: x"),
            "twice in a tick is the same tick"
        );
        notes.tick();
        assert!(!notes.fresh("warning: x"), "repeated");
        assert!(notes.fresh("skip: y"));
        notes.tick();
        notes.tick(); // a tick without it
        assert!(notes.fresh("warning: x"), "back after a pause");
    }
}
