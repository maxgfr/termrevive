//! Terminal emulators. A backend reads its window → tab → pane tree and replays
//! a restore plan; processes, Claude sessions, rules and storage are shared.
//!
//! Adding a terminal: implement [`Backend`] in its own module, list it in
//! [`registry`], and cover its parsing and restore generation with fixture
//! tests (see CONTRIBUTING.md). `applescript`, `conf` and `defaults` hold the
//! helpers the existing backends share.

pub mod applescript;
pub mod conf;
pub mod defaults;
pub mod ghostty;
pub mod iterm2;
pub mod kitty;
pub mod terminal;

use crate::procs::Proc;
use crate::rules::Launch;
use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct RawLayout {
    pub windows: Vec<RawWindow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawWindow {
    pub tabs: Vec<RawTab>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawTab {
    pub title: String,
    pub selected: bool,
    pub terminals: Vec<RawTerminal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawTerminal {
    pub id: String,
    pub title: String,
    /// What the terminal reports; may be empty.
    pub cwd: String,
    /// The pane's tty (`ttys004`) when the terminal reports it. Panes with a
    /// tty are paired with their shell exactly; the others by score.
    pub tty: Option<String>,
}

pub struct WindowPlan {
    pub tabs: Vec<TabPlan>,
}

pub struct TabPlan {
    pub selected: bool,
    pub panes: Vec<Launch>,
}

/// What a terminal can rebuild.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Panes side by side in a tab. Without, each extra pane becomes a tab.
    pub splits: bool,
}

/// A terminal setting that has to hold for restores to work.
pub struct Setting {
    pub name: &'static str,
    /// `None`: not set anywhere, the terminal's default applies.
    pub current: Option<String>,
    pub wanted: &'static str,
    pub ok: bool,
    /// What goes wrong otherwise.
    pub why: &'static str,
}

pub trait Backend {
    /// Stable id, stored in snapshots and accepted by `--backend`.
    fn name(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn caps(&self) -> Caps;
    fn installed(&self) -> bool;
    /// Emulator processes; the shells of a terminal that reports no tty are
    /// found among their children. Empty: not running.
    fn host_pids(&self, procs: &[Proc]) -> Vec<i32>;
    fn running(&self, procs: &[Proc]) -> bool {
        !self.host_pids(procs).is_empty()
    }
    fn read_layout(&self, procs: &[Proc]) -> Result<RawLayout>;
    /// Starts the emulator if needed and waits until it accepts commands.
    fn ensure_running(&self) -> Result<()>;
    /// Ids of the current windows that hold a single pane (a fresh app's
    /// default window, closed after an unattended restore).
    fn blank_windows(&self) -> Result<Vec<String>>;
    fn close_windows(&self, ids: &[String]) -> Result<()>;
    /// What `restore_window` would run, for `--dry-run`.
    fn restore_script(&self, window: &WindowPlan, delay_ms: u64) -> String;
    fn restore_window(&self, window: &WindowPlan, delay_ms: u64) -> Result<()>;
    /// Settings of the terminal itself that restores depend on.
    fn settings(&self) -> Vec<Setting> {
        Vec::new()
    }
    /// Writes `setting.wanted`; returns the file changed.
    fn apply_setting(&self, setting: &Setting) -> Result<PathBuf> {
        bail!("{} cannot set {}", self.display_name(), setting.name)
    }
}

/// Every supported terminal.
pub fn registry() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(ghostty::Ghostty),
        Box::new(iterm2::Iterm2),
        Box::new(kitty::Kitty),
        Box::new(terminal::Terminal),
    ]
}

pub fn by_name(name: &str) -> Result<Box<dyn Backend>> {
    match registry().into_iter().find(|b| b.name() == name) {
        Some(b) => Ok(b),
        None => bail!("unknown terminal `{name}` (available: {})", names()),
    }
}

/// `auto`: every terminal; otherwise the one named.
pub fn select(name: &str) -> Result<Vec<Box<dyn Backend>>> {
    if name == "auto" {
        Ok(registry())
    } else {
        Ok(vec![by_name(name)?])
    }
}

pub fn names() -> String {
    registry()
        .iter()
        .map(|b| b.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `/dev/ttys004` → `ttys004`, as `ps` prints it.
pub fn short_tty(tty: &str) -> Option<String> {
    let t = tty.trim().trim_start_matches("/dev/");
    (!t.is_empty()).then(|| t.to_string())
}

/// First line typed into a new pane when the terminal cannot open it in a
/// given directory. The leading space keeps it out of the shell history
/// (zsh `HIST_IGNORE_SPACE`, bash `ignorespace`). The screen is then cleared
/// with a plain escape sequence: `clear` also wipes the scrollback, which
/// iTerm2 blocks behind a permission prompt.
pub fn cd_line(cwd: &str) -> String {
    format!(
        " cd {} && printf '\\033[H\\033[2J'",
        crate::rules::shell_quote(cwd)
    )
}
