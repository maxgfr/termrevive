//! iTerm2 through its AppleScript dictionary. Sessions report their tty, so
//! panes are paired with their shell exactly.

use super::applescript as osa;
use super::defaults;
use super::{
    Backend, Caps, RawLayout, RawTab, RawTerminal, RawWindow, Setting, WindowPlan, cd_line,
};
use crate::procs::{self, Proc};
use crate::rules::Launch;
use anyhow::Result;
use std::path::PathBuf;

pub struct Iterm2;

const BUNDLE: &str = "com.googlecode.iterm2";
const EXE_SUFFIX: &str = "iTerm.app/Contents/MacOS/iTerm2";

const READ_SCRIPT: &str = r#"
set US to character id 31
set RS to character id 30
set out to ""
tell application "iTerm2"
	repeat with w in windows
		set cur to ""
		try
			set cur to id of current session of w
		end try
		set out to out & "W" & US & (id of w) & US & cur & RS
		repeat with t in tabs of w
			set ttl to ""
			try
				set ttl to my txt(title of t)
			end try
			set out to out & "T" & US & ttl & RS
			repeat with s in sessions of t
				set out to out & "S" & US & (id of s) & US & my txt(name of s) & US & my txt(tty of s) & RS
			end repeat
		end repeat
	end repeat
end tell
return out
"#;

const BLANK_SCRIPT: &str = r#"
set out to ""
tell application "iTerm2"
	repeat with w in windows
		if (count of tabs of w) is 1 and (count of sessions of current tab of w) is 1 then
			set out to out & (id of w) & linefeed
		end if
	end repeat
end tell
return out
"#;

fn parse_layout(text: &str) -> RawLayout {
    let mut windows: Vec<RawWindow> = Vec::new();
    let mut current = String::new();
    for f in osa::records(text) {
        match f[0] {
            "W" => {
                current = f.get(2).unwrap_or(&"").to_string();
                windows.push(RawWindow { tabs: Vec::new() });
            }
            "T" => {
                if let Some(w) = windows.last_mut() {
                    w.tabs.push(RawTab {
                        title: f.get(1).unwrap_or(&"").to_string(),
                        selected: false,
                        terminals: Vec::new(),
                    });
                }
            }
            "S" if f.len() >= 4 => {
                if let Some(t) = windows.last_mut().and_then(|w| w.tabs.last_mut()) {
                    // The tab holding the window's current session is the selected one.
                    t.selected |= !current.is_empty() && f[1] == current;
                    if t.title.is_empty() {
                        t.title = f[2].to_string();
                    }
                    t.terminals.push(RawTerminal {
                        id: f[1].to_string(),
                        title: f[2].to_string(),
                        cwd: String::new(),
                        tty: Some(f[3].to_string()).filter(|s| !s.is_empty()),
                    });
                }
            }
            _ => {}
        }
    }
    windows.retain(|w| !w.tabs.is_empty());
    RawLayout { windows }
}

/// `write text` statements bringing a new session `p` to `l`.
fn fill(l: &Launch) -> String {
    let mut s = format!("\ttell p to write text {}\n", osa::str(&cd_line(&l.cwd)));
    if let Some(input) = &l.input {
        let newline = if l.run { "" } else { " newline NO" };
        s += &format!("\ttell p to write text {}{newline}\n", osa::str(input));
    }
    s
}

impl Backend for Iterm2 {
    fn name(&self) -> &'static str {
        "iterm2"
    }

    fn display_name(&self) -> &'static str {
        "iTerm2"
    }

    fn caps(&self) -> Caps {
        Caps { splits: true }
    }

    fn installed(&self) -> bool {
        osa::app_installed("iTerm.app")
    }

    fn host_pids(&self, procs: &[Proc]) -> Vec<i32> {
        procs::find_by_exe(procs, EXE_SUFFIX)
            .iter()
            .map(|p| p.pid)
            .collect()
    }

    fn read_layout(&self, _procs: &[Proc]) -> Result<RawLayout> {
        Ok(parse_layout(&osa::run(&format!(
            "{}{READ_SCRIPT}",
            osa::TXT_HANDLER
        ))?))
    }

    fn ensure_running(&self) -> Result<()> {
        if !self.running(&procs::process_table()?) {
            osa::launch(BUNDLE)?;
        }
        osa::wait_ready(r#"tell application "iTerm2" to count windows"#)
    }

    fn blank_windows(&self) -> Result<Vec<String>> {
        Ok(osa::run(BLANK_SCRIPT)?
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    fn close_windows(&self, ids: &[String]) -> Result<()> {
        let body: String = ids
            .iter()
            .filter(|id| id.parse::<i64>().is_ok())
            .map(|id| format!("\ttry\n\t\tclose (first window whose id is {id})\n\tend try\n"))
            .collect();
        if body.is_empty() {
            return Ok(());
        }
        osa::run(&format!("tell application \"iTerm2\"\n{body}end tell")).map(drop)
    }

    fn restore_script(&self, window: &WindowPlan, delay_ms: u64) -> String {
        let mut s = String::from("tell application \"iTerm2\"\n");
        let mut selected = None;
        let mut have_window = false;
        for (ti, tab) in window.tabs.iter().enumerate() {
            let Some((first, rest)) = tab.panes.split_first() else {
                continue;
            };
            if !have_window {
                have_window = true;
                s += &format!(
                    "\tset w to (create window with default profile)\n\tset t{ti} to current tab of w\n"
                );
            } else {
                s += &osa::delay(delay_ms);
                s += &format!("\ttell w to set t{ti} to (create tab with default profile)\n");
            }
            s += &format!("\tset p to current session of t{ti}\n");
            s += &fill(first);
            for pane in rest {
                s += &osa::delay(delay_ms);
                s += "\ttell p to set q to (split vertically with default profile)\n\tset p to q\n";
                s += &fill(pane);
            }
            if tab.selected {
                selected = Some(ti);
            }
        }
        if let Some(ti) = selected {
            s += &format!("\ttell t{ti} to select\n");
        }
        s += "end tell\n";
        s
    }

    fn restore_window(&self, window: &WindowPlan, delay_ms: u64) -> Result<()> {
        if window.tabs.iter().all(|t| t.panes.is_empty()) {
            return Ok(());
        }
        osa::run(&self.restore_script(window, delay_ms)).map(drop)
    }

    fn settings(&self) -> Vec<Setting> {
        let current = defaults::read(BUNDLE, defaults::IGNORE_STATE);
        vec![Setting {
            name: defaults::IGNORE_STATE,
            ok: current.as_deref() == Some("1"),
            current,
            wanted: "1",
            why: "macOS reopens iTerm2's own windows at launch, on top of a restore",
        }]
    }

    fn apply_setting(&self, setting: &Setting) -> Result<PathBuf> {
        defaults::write_bool(BUNDLE, setting.name, true)?;
        Ok(defaults::plist_path(BUNDLE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TabPlan;
    use crate::backend::applescript::{RS, US};

    #[test]
    fn parses_sessions_ttys_and_the_current_tab() {
        let text = format!(
            "W{US}11{US}S2{RS}T{US}{RS}S{US}S1{US}zsh{US}/dev/ttys010{RS}\
             T{US}{RS}S{US}S2{US}✳ fix{US}/dev/ttys011{RS}S{US}S3{US}less{US}/dev/ttys012{RS}\
             W{US}12{US}{RS}"
        );
        let l = parse_layout(&text);
        assert_eq!(l.windows.len(), 1, "the empty window is dropped");
        let tabs = &l.windows[0].tabs;
        assert!(!tabs[0].selected && tabs[1].selected);
        assert_eq!(tabs[1].title, "✳ fix");
        assert_eq!(tabs[1].terminals[1].tty.as_deref(), Some("/dev/ttys012"));
    }

    #[test]
    fn builds_windows_tabs_splits_with_cd_and_input() {
        let l = |cwd: &str, input: Option<&str>, run| Launch {
            cwd: cwd.into(),
            input: input.map(Into::into),
            run,
        };
        let plan = WindowPlan {
            tabs: vec![
                TabPlan {
                    selected: false,
                    panes: vec![
                        l("/a b", Some("claude --resume x"), true),
                        l("/b", None, false),
                    ],
                },
                TabPlan {
                    selected: true,
                    panes: vec![l("/c", Some("make"), false)],
                },
            ],
        };
        let s = Iterm2.restore_script(&plan, 100);
        assert!(s.contains("set w to (create window with default profile)"));
        assert!(s.contains(r#"tell p to write text " cd '/a b' && printf '\\033[H\\033[2J'""#));
        assert!(s.contains("tell p to write text \"claude --resume x\"\n"));
        assert!(s.contains("tell p to set q to (split vertically with default profile)"));
        assert!(s.contains("tell w to set t1 to (create tab with default profile)"));
        assert!(s.contains(r#"tell p to write text "make" newline NO"#));
        assert!(s.contains("tell t1 to select"));
    }
}
