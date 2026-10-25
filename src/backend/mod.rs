//! Terminal emulators. Each backend reads its window/tab/split tree and replays
//! a restore plan; everything else (processes, rules, storage) is shared.

pub mod ghostty;

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
    pub cwd: String,
}

pub struct WindowPlan {
    pub tabs: Vec<TabPlan>,
}

pub struct TabPlan {
    pub selected: bool,
    pub panes: Vec<Launch>,
}

/// A terminal setting that has to hold for restores to work.
pub struct Setting {
    pub name: &'static str,
    /// `None`: not set anywhere, the terminal's default applies.
    pub current: Option<String>,
    pub wanted: &'static str,
    /// What goes wrong otherwise.
    pub why: &'static str,
}

impl Setting {
    pub fn ok(&self) -> bool {
        self.current.as_deref() == Some(self.wanted)
    }
}

pub trait Backend {
    fn name(&self) -> &'static str;
    /// Emulator processes whose children are the shells (empty: not running).
    fn host_pids(&self, procs: &[Proc]) -> Vec<i32>;
    fn read_layout(&self) -> Result<RawLayout>;
    /// Starts the emulator if needed and waits until it accepts commands.
    fn ensure_running(&self) -> Result<()>;
    /// Ids of the current windows that hold a single idle-looking terminal.
    fn blank_windows(&self) -> Result<Vec<String>>;
    fn close_windows(&self, ids: &[String]) -> Result<()>;
    /// The script `restore_window` would run, for `--dry-run`.
    fn restore_script(&self, window: &WindowPlan, delay_ms: u64) -> String;
    fn restore_window(&self, window: &WindowPlan, delay_ms: u64) -> Result<()>;
    /// Settings of the terminal itself that restores depend on.
    fn settings(&self) -> Vec<Setting> {
        Vec::new()
    }
    /// Writes `setting.wanted` into the terminal's config; returns the file changed.
    fn apply_setting(&self, setting: &Setting) -> Result<PathBuf> {
        bail!("{} cannot set {}", self.name(), setting.name)
    }
}

pub fn by_name(name: &str) -> Result<Box<dyn Backend>> {
    match name {
        "ghostty" => Ok(Box::new(ghostty::Ghostty)),
        other => bail!("unknown terminal backend `{other}` (available: ghostty)"),
    }
}
