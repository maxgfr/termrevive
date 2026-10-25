use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 2;

/// A terminal-agnostic picture of every window, tab and pane at one point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: DateTime<Local>,
    /// `kern.boottime` (seconds) of the boot during which the snapshot was taken.
    pub boot_time: i64,
    /// Version 1 snapshots name their single terminal here; version 2 ones
    /// name it on each window. [`Snapshot::normalize`] moves it down.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// The terminal it belongs to (`ghostty`, `iterm2`…).
    #[serde(default)]
    pub backend: String,
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
    /// Upgrades a version 1 snapshot in memory.
    pub fn normalize(mut self) -> Snapshot {
        let legacy = std::mem::take(&mut self.backend);
        for w in &mut self.windows {
            if w.backend.is_empty() {
                w.backend = legacy.clone();
            }
        }
        self
    }

    /// Terminals present, in order of first appearance.
    pub fn backends(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.windows {
            if !out.contains(&w.backend) {
                out.push(w.backend.clone());
            }
        }
        out
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_1_snapshots_load_with_their_terminal_on_every_window() {
        let v1 = r#"{"version":1,"taken_at":"2026-10-25T10:43:04+01:00","boot_time":1,
            "backend":"ghostty","windows":[{"tabs":[]},{"tabs":[],"orphans":true}]}"#;
        let s = serde_json::from_str::<Snapshot>(v1).unwrap().normalize();
        assert!(s.backend.is_empty());
        assert_eq!(s.backends(), vec!["ghostty"]);
        assert!(s.windows.iter().all(|w| w.backend == "ghostty"));
        let v2 = serde_json::to_string(&s).unwrap();
        assert!(!v2.contains(r#""backend":"ghostty","windows""#));
    }
}
