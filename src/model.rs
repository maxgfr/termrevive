use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 3;

/// A terminal-agnostic picture of every window, tab and pane at one point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: DateTime<Local>,
    /// `kern.boottime` (seconds) of the boot during which the snapshot was taken.
    pub boot_time: i64,
    /// `kern.bootsessionuuid` of that boot; absent from snapshots of
    /// termrevive ≤ 1.3.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_session: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Program {
    /// Basename of the executable, used to match rules.
    pub name: String,
    pub argv: Vec<String>,
    /// The agent session this program works on (Claude Code, Codex…), when
    /// its harness could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSession>,
    /// Version ≤ 2 snapshots: a Claude Code session, moved into `agent` by
    /// [`Snapshot::normalize`].
    #[serde(default, skip_serializing)]
    pub claude: Option<LegacyClaude>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Harness name (`claude`, `codex`, `vibe`…).
    pub harness: String,
    pub session_id: String,
    /// Where the session belongs; empty when unknown.
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyClaude {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// One boot of the machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Boot {
    /// `kern.boottime`, in seconds. It is derived from the wall clock, so a
    /// clock step can shift it.
    pub time: i64,
    /// `kern.bootsessionuuid`: exact, when known.
    pub session: Option<String>,
}

impl Boot {
    /// Whether both are the same boot: by session when both know theirs,
    /// else by boot time.
    pub fn same(&self, other: &Boot) -> bool {
        match (&self.session, &other.session) {
            (Some(a), Some(b)) => a == b,
            _ => self.time == other.time,
        }
    }
}

impl Snapshot {
    /// The boot during which it was taken.
    pub fn boot(&self) -> Boot {
        Boot {
            time: self.boot_time,
            session: self.boot_session.clone(),
        }
    }

    /// Upgrades an older snapshot in memory: v1's single terminal moves onto
    /// each window, v1/v2's Claude session becomes an agent session.
    pub fn normalize(mut self) -> Snapshot {
        let legacy = std::mem::take(&mut self.backend);
        for w in &mut self.windows {
            if w.backend.is_empty() {
                w.backend = legacy.clone();
            }
            for p in w.tabs.iter_mut().flat_map(|t| &mut t.panes) {
                if let Some(prog) = &mut p.program
                    && let Some(c) = prog.claude.take()
                {
                    prog.agent.get_or_insert(AgentSession {
                        harness: "claude".into(),
                        session_id: c.session_id,
                        cwd: c.cwd,
                        name: c.name,
                    });
                }
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

    /// Agent sessions per harness, in order of first appearance.
    pub fn agent_counts(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for a in self
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| &t.panes)
            .filter_map(|p| p.program.as_ref()?.agent.as_ref())
        {
            match out.iter_mut().find(|(h, _)| *h == a.harness) {
                Some((_, n)) => *n += 1,
                None => out.push((a.harness.clone(), 1)),
            }
        }
        out
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

    #[test]
    fn version_2_claude_sessions_become_agent_sessions() {
        let v2 = r#"{"version":2,"taken_at":"2026-10-25T10:43:04+01:00","boot_time":1,
            "windows":[{"backend":"ghostty","tabs":[{"title":"t","selected":true,"panes":[
              {"title":"t","cwd":"/d","program":{"name":"claude","argv":["claude"],
               "claude":{"session_id":"S","cwd":"/d/w","name":"fix"}}}]}]}]}"#;
        let s = serde_json::from_str::<Snapshot>(v2).unwrap().normalize();
        let p = s.windows[0].tabs[0].panes[0].program.as_ref().unwrap();
        assert_eq!(
            p.agent,
            Some(AgentSession {
                harness: "claude".into(),
                session_id: "S".into(),
                cwd: "/d/w".into(),
                name: Some("fix".into()),
            })
        );
        assert_eq!(s.agent_counts(), vec![("claude".to_string(), 1)]);
        let v3 = serde_json::to_string(&s).unwrap();
        assert!(v3.contains(r#""agent":{"harness":"claude""#) && !v3.contains(r#""claude":{"#));
        assert!(!v3.contains("boot_session"), "unknown, so not written");
    }

    #[test]
    fn boots_are_told_apart_by_session_when_both_know_it() {
        let b = |time, session: Option<&str>| Boot {
            time,
            session: session.map(String::from),
        };
        // A clock step moved the boot time: still the same boot.
        assert!(b(100, Some("A")).same(&b(103, Some("A"))));
        // Same second, another boot.
        assert!(!b(100, Some("A")).same(&b(100, Some("B"))));
        // A snapshot of termrevive ≤ 1.3.1: by time.
        assert!(b(100, None).same(&b(100, Some("A"))));
        assert!(!b(100, None).same(&b(200, Some("A"))));
    }
}
