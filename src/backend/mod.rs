//! Terminal emulators. Each backend reads its window/tab/split tree and replays
//! a restore plan; everything else (processes, rules, storage) is shared.

pub mod ghostty;

use crate::procs::Proc;
use crate::rules::Launch;
use anyhow::{Result, bail};

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
}

pub fn by_name(name: &str) -> Result<Box<dyn Backend>> {
    match name {
        "ghostty" => Ok(Box::new(ghostty::Ghostty)),
        other => bail!("unknown terminal backend `{other}` (available: ghostty)"),
    }
}
