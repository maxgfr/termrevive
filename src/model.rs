use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 1;

/// A terminal-agnostic picture of every window, tab and pane at one point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: DateTime<Local>,
    /// `kern.boottime` (seconds) of the boot during which the snapshot was taken.
    pub boot_time: i64,
    pub backend: String,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub tabs: Vec<Tab>,
    /// Windows holding programs that could not be tied to a visible terminal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orphans: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub title: String,
    pub selected: bool,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub title: String,
    pub cwd: String,
    /// Foreground program; `None` means an idle shell.
    pub program: Option<Program>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    /// Basename of the executable, used to match rules.
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Snapshot {
    pub fn pane_count(&self) -> usize {
        self.windows
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| t.panes.len())
            .sum()
    }

    pub fn tab_count(&self) -> usize {
        self.windows.iter().map(|w| w.tabs.len()).sum()
    }

    pub fn claude_count(&self) -> usize {
        self.windows
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .filter(|p| p.program.as_ref().is_some_and(|p| p.claude.is_some()))
            .count()
    }
}
