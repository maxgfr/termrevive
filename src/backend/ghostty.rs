//! Ghostty (macOS) through its AppleScript dictionary (Ghostty ≥ 1.3).
//! Ghostty reports no tty: panes are paired with shells by score.

use super::applescript as osa;
use super::conf::{self, Syntax};
use super::{Backend, Caps, RawLayout, RawTab, RawTerminal, RawWindow, Setting, WindowPlan};
use crate::procs::{self, Proc};
use crate::rules::Launch;
use anyhow::Result;
use std::path::PathBuf;

pub struct Ghostty;

const BUNDLE: &str = "com.mitchellh.ghostty";
const EXE_SUFFIX: &str = "Ghostty.app/Contents/MacOS/ghostty";
const SAVE_STATE: &str = "window-save-state";

const READ_SCRIPT: &str = r#"
set US to character id 31
set RS to character id 30
set out to ""
tell application "Ghostty"
	repeat with w in windows
		set out to out & "W" & US & (id of w) & RS
		repeat with t in tabs of w
			set out to out & "T" & US & my txt(selected of t) & US & my txt(name of t) & RS
			repeat with s in terminals of t
				set out to out & "S" & US & (id of s) & US & my txt(name of s) & US & my txt(working directory of s) & RS
			end repeat
		end repeat
	end repeat
end tell
return out
"#;

const BLANK_SCRIPT: &str = r#"
set out to ""
tell application "Ghostty"
	repeat with w in windows
		if (count of tabs of w) is 1 and (count of terminals of w) is 1 then
			set out to out & (id of w) & linefeed
		end if
	end repeat
end tell
return out
"#;

/// Ghostty's config files on macOS, in load order (a later one overrides).
fn config_files() -> Vec<PathBuf> {
    let (Some(xdg), Some(home)) = (conf::xdg_dir("ghostty"), dirs::home_dir()) else {
        return Vec::new();
    };
    let support = home.join("Library/Application Support/com.mitchellh.ghostty");
    vec![
        xdg.join("config"),
        xdg.join("config.ghostty"),
        support.join("config"),
        support.join("config.ghostty"),
    ]
}

fn parse_layout(text: &str) -> RawLayout {
    let mut windows: Vec<RawWindow> = Vec::new();
    for f in osa::records(text) {
        match f[0] {
            "W" => windows.push(RawWindow { tabs: Vec::new() }),
            "T" if f.len() >= 3 => {
                if let Some(w) = windows.last_mut() {
                    w.tabs.push(RawTab {
                        selected: f[1] == "true",
                        title: f[2].to_string(),
                        terminals: Vec::new(),
                    });
                }
            }
            "S" if f.len() >= 4 => {
                if let Some(t) = windows.last_mut().and_then(|w| w.tabs.last_mut()) {
                    t.terminals.push(RawTerminal {
                        id: f[1].to_string(),
                        title: f[2].to_string(),
                        cwd: f[3].to_string(),
                        tty: None,
                    });
                }
            }
            _ => {}
        }
    }
    RawLayout { windows }
}

fn surface_config(l: &Launch) -> String {
    let mut fields = vec![format!("initial working directory:{}", osa::str(&l.cwd))];
    if let Some(input) = &l.input {
        let enter = if l.run { " & return" } else { "" };
        fields.push(format!("initial input:{}{enter}", osa::str(input)));
    }
    format!("{{{}}}", fields.join(", "))
}

impl Backend for Ghostty {
    fn name(&self) -> &'static str {
        "ghostty"
    }

    fn display_name(&self) -> &'static str {
        "Ghostty"
    }

    fn caps(&self) -> Caps {
        Caps { splits: true }
    }

    fn installed(&self) -> bool {
        osa::app_installed("Ghostty.app")
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
        osa::wait_ready(r#"tell application "Ghostty" to count windows"#)
    }

    fn blank_windows(&self) -> Result<Vec<String>> {
        Ok(osa::run(BLANK_SCRIPT)?
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    fn close_windows(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let body: String = ids
            .iter()
            .map(|id| {
                format!(
                    "\ttry\n\t\tclose window (window id {})\n\tend try\n",
                    osa::str(id)
                )
            })
            .collect();
        osa::run(&format!("tell application \"Ghostty\"\n{body}end tell")).map(drop)
    }

    fn restore_script(&self, window: &WindowPlan, delay_ms: u64) -> String {
        let mut s = String::from("tell application \"Ghostty\"\n");
        let mut selected = None;
        let mut have_window = false;
        for (ti, tab) in window.tabs.iter().enumerate() {
            let Some((first, rest)) = tab.panes.split_first() else {
                continue;
            };
            if !have_window {
                have_window = true;
                s += &format!(
                    "\tset w to new window with configuration {}\n\tset t{ti} to tab 1 of w\n",
                    surface_config(first)
                );
            } else {
                s += &osa::delay(delay_ms);
                s += &format!(
                    "\tset t{ti} to new tab in w with configuration {}\n",
                    surface_config(first)
                );
            }
            s += &format!("\tset p to terminal 1 of t{ti}\n");
            for pane in rest {
                s += &osa::delay(delay_ms);
                s += &format!(
                    "\tset p to split p direction right with configuration {}\n",
                    surface_config(pane)
                );
            }
            if tab.selected {
                selected = Some(ti);
            }
        }
        if let Some(ti) = selected {
            s += &format!("\tselect tab t{ti}\n");
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
        let current = conf::read_key(&config_files(), SAVE_STATE, Syntax::Equals);
        vec![Setting {
            name: SAVE_STATE,
            ok: current.as_deref() == Some("never"),
            current,
            wanted: "never",
            why: "Ghostty reopens its own windows at launch, on top of a restore",
        }]
    }

    fn apply_setting(&self, setting: &Setting) -> Result<PathBuf> {
        conf::write_key(
            &config_files(),
            setting.name,
            setting.wanted,
            Syntax::Equals,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TabPlan;
    use crate::backend::applescript::{RS, US};

    #[test]
    fn parses_read_script_output() {
        let text = format!(
            "W{US}w1{RS}T{US}true{US}✳ a{RS}S{US}id1{US}✳ a{US}/x{RS}S{US}id2{US}{US}/y{RS}\
             T{US}false{US}b{RS}S{US}id3{US}b{US}/z{RS}W{US}w2{RS}T{US}true{US}c{RS}S{US}id4{US}c{US}{RS}"
        );
        let l = parse_layout(&text);
        assert_eq!(l.windows.len(), 2);
        assert_eq!(l.windows[0].tabs.len(), 2);
        assert!(l.windows[0].tabs[0].selected);
        assert_eq!(l.windows[0].tabs[0].terminals[1].cwd, "/y");
        assert_eq!(l.windows[0].tabs[0].terminals[1].title, "");
        assert_eq!(l.windows[1].tabs[0].terminals[0].cwd, "");
        assert_eq!(l.windows[1].tabs[0].terminals[0].tty, None);
    }

    #[test]
    fn builds_tabs_splits_and_selection() {
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
                        l("/a", Some("claude --resume x"), true),
                        l("/b", None, false),
                    ],
                },
                TabPlan {
                    selected: true,
                    panes: vec![l("/c", Some("make"), false)],
                },
            ],
        };
        let s = Ghostty.restore_script(&plan, 250);
        assert!(s.contains(
            r#"set w to new window with configuration {initial working directory:"/a", initial input:"claude --resume x" & return}"#
        ));
        assert!(s.contains(
            r#"set p to split p direction right with configuration {initial working directory:"/b"}"#
        ));
        assert!(s.contains(
            r#"set t1 to new tab in w with configuration {initial working directory:"/c", initial input:"make"}"#
        ));
        assert!(s.contains("delay 0.250"));
        assert!(s.contains("select tab t1"));
    }
}
