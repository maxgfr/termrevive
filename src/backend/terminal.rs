//! Terminal.app through its AppleScript dictionary. Tabs report their tty
//! (exact pairing); there are no splits. With macOS window tabbing, AppleScript
//! shows each Terminal tab as a window of its own and has no way to open a tab,
//! so every tab is saved, and restored, as a window.

use super::applescript as osa;
use super::defaults;
use super::{
    Backend, Caps, RawLayout, RawTab, RawTerminal, RawWindow, Setting, WindowPlan, cd_line,
};
use crate::procs::{self, Proc};
use crate::rules::{Launch, shell_quote};
use anyhow::Result;
use std::path::PathBuf;

pub struct Terminal;

const BUNDLE: &str = "com.apple.Terminal";
const EXE_SUFFIX: &str = "Terminal.app/Contents/MacOS/Terminal";

const READ_SCRIPT: &str = r#"
set US to character id 31
set RS to character id 30
set out to ""
tell application "Terminal"
	repeat with w in windows
		set out to out & "W" & US & (id of w) & US & my txt(name of w) & RS
		repeat with t in tabs of w
			set out to out & "T" & US & my txt(selected of t) & US & my txt(custom title of t) & US & my txt(tty of t) & RS
		end repeat
	end repeat
end tell
return out
"#;

const BLANK_SCRIPT: &str = r#"
set out to ""
tell application "Terminal"
	repeat with w in windows
		if (count of tabs of w) is 1 then set out to out & (id of w) & linefeed
	end repeat
end tell
return out
"#;

fn parse_layout(text: &str) -> RawLayout {
    let mut windows: Vec<RawWindow> = Vec::new();
    let mut window_name = String::new();
    for f in osa::records(text) {
        match f[0] {
            "W" => {
                window_name = f.get(2).unwrap_or(&"").to_string();
                windows.push(RawWindow { tabs: Vec::new() });
            }
            "T" if f.len() >= 4 => {
                if let Some(w) = windows.last_mut() {
                    let selected = f[1] == "true";
                    let title = if !f[2].is_empty() {
                        f[2].to_string()
                    } else if selected {
                        window_name.clone()
                    } else {
                        String::new()
                    };
                    w.tabs.push(RawTab {
                        title: title.clone(),
                        selected,
                        terminals: vec![RawTerminal {
                            id: f[3].to_string(),
                            title,
                            cwd: String::new(),
                            tty: Some(f[3].to_string()).filter(|s| !s.is_empty()),
                        }],
                    });
                }
            }
            _ => {}
        }
    }
    windows.retain(|w| !w.tabs.is_empty());
    RawLayout { windows }
}

/// The login shell's name (`zsh`, `bash`…), from the user database.
fn login_shell() -> String {
    // SAFETY: getpwuid returns a pointer into static storage, read at once.
    let shell = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*pw).pw_shell)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    shell
        .or_else(|| std::env::var("SHELL").ok())
        .map(|s| procs::basename(&s).to_string())
        .unwrap_or_else(|| "zsh".into())
}

/// The line that runs `l`'s command, or leaves it typed: Terminal.app can only
/// send whole lines, so "type without running" goes through the shell's own
/// line buffer (`print -z` in zsh) or history (`history -s` in bash).
fn command_line(l: &Launch, shell: &str) -> Option<String> {
    let input = l.input.as_ref()?;
    if l.run {
        return Some(input.clone());
    }
    Some(match shell {
        "zsh" => format!(" print -z -- {}", shell_quote(input)),
        "bash" => format!(" history -s -- {}", shell_quote(input)),
        _ => format!(" # {input}"),
    })
}

/// The single line typed into a new window: Terminal.app runs it as soon as the
/// shell reads input, so the directory change and the command travel together.
fn line(l: &Launch, shell: &str) -> String {
    let cd = cd_line(&l.cwd);
    match command_line(l, shell) {
        Some(cmd) => format!("{cd};{cmd}"),
        None => cd,
    }
}

impl Terminal {
    fn script(&self, window: &WindowPlan, delay_ms: u64, shell: &str) -> String {
        let mut s = String::from("tell application \"Terminal\"\n");
        let lines: Vec<String> = window
            .tabs
            .iter()
            .filter_map(|t| t.panes.first())
            .map(|l| line(l, shell))
            .collect();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                s += &osa::delay(delay_ms);
            }
            s += &format!("\tdo script {}\n", osa::str(l));
        }
        s += "end tell\n";
        s
    }
}

impl Backend for Terminal {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn display_name(&self) -> &'static str {
        "Terminal.app"
    }

    fn caps(&self) -> Caps {
        Caps { splits: false }
    }

    fn installed(&self) -> bool {
        osa::app_installed("Terminal.app")
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
        // `launch` starts Terminal without its default window; with `open`, the
        // first `do script` races that window and its text gets lost.
        osa::wait_ready(
            r#"tell application "Terminal" to launch
tell application "Terminal" to count windows"#,
        )
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
        osa::run(&format!("tell application \"Terminal\"\n{body}end tell")).map(drop)
    }

    fn restore_script(&self, window: &WindowPlan, delay_ms: u64) -> String {
        self.script(window, delay_ms, &login_shell())
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
            why: "macOS reopens Terminal.app's own windows at launch, on top of a restore",
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
    fn parses_tabs_with_their_tty() {
        let text = format!(
            "W{US}301{US}maxime — -zsh — 80×24{RS}T{US}false{US}{US}/dev/ttys020{RS}\
             T{US}true{US}{US}/dev/ttys021{RS}W{US}302{US}x{RS}"
        );
        let l = parse_layout(&text);
        assert_eq!(l.windows.len(), 1);
        let tabs = &l.windows[0].tabs;
        assert_eq!(tabs.len(), 2);
        assert!(tabs[1].selected);
        assert_eq!(tabs[1].title, "maxime — -zsh — 80×24");
        assert_eq!(tabs[0].terminals[0].tty.as_deref(), Some("/dev/ttys020"));
    }

    #[test]
    fn types_without_running_through_the_shell() {
        let l = Launch {
            cwd: "/".into(),
            input: Some("make deploy".into()),
            run: false,
        };
        assert_eq!(
            command_line(&l, "zsh").unwrap(),
            " print -z -- 'make deploy'"
        );
        assert_eq!(
            command_line(&l, "bash").unwrap(),
            " history -s -- 'make deploy'"
        );
        let run = Launch { run: true, ..l };
        assert_eq!(command_line(&run, "zsh").unwrap(), "make deploy");
    }

    #[test]
    fn opens_one_window_per_tab_with_cd_and_command_on_one_line() {
        let l = |cwd: &str, input: Option<&str>, run| Launch {
            cwd: cwd.into(),
            input: input.map(Into::into),
            run,
        };
        let plan = WindowPlan {
            tabs: vec![
                TabPlan {
                    selected: false,
                    panes: vec![l("/a b", Some("claude --resume x"), true)],
                },
                TabPlan {
                    selected: true,
                    panes: vec![l("/b", None, false)],
                },
            ],
        };
        let s = Terminal.script(&plan, 100, "zsh");
        assert!(
            s.contains(r#"do script " cd '/a b' && printf '\\033[H\\033[2J';claude --resume x""#)
        );
        assert!(s.contains(r#"do script " cd /b && printf '\\033[H\\033[2J'""#));
        assert_eq!(s.matches("do script").count(), 2);
    }
}
