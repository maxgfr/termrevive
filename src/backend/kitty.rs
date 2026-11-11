//! kitty through its remote control (`kitten @`). kitty must listen on a unix
//! socket with remote control allowed (`termrevive doctor --fix` sets both in
//! kitty.conf); the socket is found from the running kitty process itself, so
//! `--listen-on` on the command line works too. Windows report the shell's
//! pid, turned into a tty for exact pairing.

use super::applescript as osa;
use super::conf::{self, Syntax};
use super::{Backend, Caps, RawLayout, RawTab, RawTerminal, RawWindow, Setting, WindowPlan};
use crate::exec;
use crate::procs::{self, Proc};
use crate::rules::{Launch, shell_quote};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// kitty, and during a restore the instance it drives.
#[derive(Default)]
pub struct Kitty {
    conn: RefCell<Option<Conn>>,
}

/// A kitty instance's remote control: the `kitten` to run and its socket.
#[derive(Clone)]
struct Conn {
    kitten: PathBuf,
    socket: String,
}

impl Conn {
    fn remote(&self, args: &[&str], stdin: Option<&str>) -> Result<String> {
        remote(&self.kitten, &self.socket, args, stdin)
    }
}

const BUNDLE: &str = "net.kovidgoyal.kitty";
const EXE_SUFFIX: &str = "kitty.app/Contents/MacOS/kitty";
const KITTEN: &str = "/Applications/kitty.app/Contents/MacOS/kitten";
const REMOTE: &str = "allow_remote_control";
const LISTEN: &str = "listen_on";

#[derive(Deserialize)]
struct OsWindow {
    tabs: Vec<KTab>,
}

#[derive(Deserialize)]
struct KTab {
    #[serde(default)]
    title: String,
    #[serde(default)]
    is_active: bool,
    windows: Vec<KWindow>,
}

#[derive(Deserialize)]
struct KWindow {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pid: i32,
    #[serde(default)]
    cwd: String,
}

fn config_files() -> Vec<PathBuf> {
    let dir = std::env::var_os("KITTY_CONFIG_DIRECTORY")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| conf::xdg_dir("kitty"));
    dir.map(|d| vec![d.join("kitty.conf")]).unwrap_or_default()
}

fn kitten(procs: &[Proc]) -> PathBuf {
    procs::find_by_exe(procs, EXE_SUFFIX)
        .first()
        .and_then(|p| Path::new(&p.comm).parent().map(|d| d.join("kitten")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(KITTEN))
}

/// Runs `kitten @ --to <socket> <args>`, with `stdin` piped in.
fn remote(kitten: &Path, socket: &str, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let out = exec::output(
        Command::new(kitten).args(["@", "--to", socket]).args(args),
        stdin,
        exec::QUICK,
    )?;
    if !out.status.success() {
        bail!(
            "kitten @ {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Every running kitty instance that answers on one of the unix sockets it
/// listens on, as (socket, `ls` output).
fn connect_all(procs: &[Proc]) -> Vec<(String, String)> {
    let kitten = kitten(procs);
    let mut found = Vec::new();
    for pid in procs::pids_by_exe(procs, EXE_SUFFIX) {
        let Ok(out) = exec::output(
            Command::new("/usr/sbin/lsof").args(["-a", "-p", &pid.to_string(), "-U", "-Fn"]),
            None,
            exec::QUICK,
        ) else {
            continue;
        };
        let answering = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('n'))
            .filter(|p| p.starts_with('/'))
            .find_map(|path| {
                let socket = format!("unix:{path}");
                remote(&kitten, &socket, &["ls"], None)
                    .ok()
                    .map(|ls| (socket, ls))
            });
        found.extend(answering);
    }
    found
}

/// The first instance that answers.
fn connect(procs: &[Proc]) -> Result<(String, String)> {
    connect_all(procs).into_iter().next().with_context(|| {
        "kitty runs but its remote control is unreachable: run `termrevive doctor --fix` \
         (allow_remote_control + listen_on in kitty.conf), then restart kitty"
    })
}

fn parse_layout(json: &str, procs: &[Proc]) -> Result<RawLayout> {
    let os_windows: Vec<OsWindow> = serde_json::from_str(json).context("parsing kitten @ ls")?;
    let tty_of = |pid: i32| {
        procs
            .iter()
            .find(|p| p.pid == pid && p.tty != "??")
            .map(|p| p.tty.clone())
    };
    let windows = os_windows
        .into_iter()
        .map(|ow| RawWindow {
            tabs: ow
                .tabs
                .into_iter()
                .map(|t| RawTab {
                    title: t.title,
                    selected: t.is_active,
                    terminals: t
                        .windows
                        .into_iter()
                        .map(|w| RawTerminal {
                            id: w.id.to_string(),
                            title: w.title,
                            cwd: w.cwd,
                            tty: tty_of(w.pid),
                        })
                        .collect(),
                })
                .filter(|t| !t.terminals.is_empty())
                .collect(),
        })
        .filter(|w: &RawWindow| !w.tabs.is_empty())
        .collect();
    Ok(RawLayout { windows })
}

/// One step of a restore; `$N` in args stands for the id step N returned.
#[derive(Debug, PartialEq)]
struct Step {
    args: Vec<String>,
    stdin: Option<String>,
}

fn steps(window: &WindowPlan) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    let mut anchor: Option<usize> = None; // step whose window opens the OS window
    let mut focus: Option<usize> = None;
    let launch = |kind: &str, cwd: &str, rel: Option<usize>, split: bool| {
        let mut args = vec!["launch".to_string(), format!("--type={kind}")];
        if let Some(r) = rel {
            args.push(format!("--match=id:${r}"));
        }
        if split {
            args.push("--location=vsplit".into());
        }
        args.push(format!("--cwd={cwd}"));
        Step { args, stdin: None }
    };
    let type_in = |at: usize, l: &Launch| {
        l.input.as_ref().map(|input| Step {
            args: vec![
                "send-text".into(),
                format!("--match=id:${at}"),
                "--stdin".into(),
            ],
            stdin: Some(format!("{input}{}", if l.run { "\r" } else { "" })),
        })
    };
    for tab in &window.tabs {
        let Some((first, rest)) = tab.panes.split_first() else {
            continue;
        };
        let tab_step = out.len();
        match anchor {
            None => {
                out.push(launch("os-window", &first.cwd, None, false));
                anchor = Some(tab_step);
            }
            Some(a) => out.push(launch("tab", &first.cwd, Some(a), false)),
        }
        out.extend(type_in(tab_step, first));
        let mut prev = tab_step;
        for pane in rest {
            let at = out.len();
            out.push(launch("window", &pane.cwd, Some(prev), true));
            out.extend(type_in(at, pane));
            prev = at;
        }
        if tab.selected {
            focus = Some(tab_step);
        }
    }
    if let Some(f) = focus {
        out.push(Step {
            args: vec!["focus-window".into(), format!("--match=id:${f}")],
            stdin: None,
        });
    }
    out
}

impl Kitty {
    /// The instance a restore drives: found once (a process table, an `lsof`
    /// and a `kitten @ ls`), then reused for every window.
    fn conn(&self) -> Result<Conn> {
        if let Some(c) = self.conn.borrow().clone() {
            return Ok(c);
        }
        let procs = procs::process_table()?;
        let (socket, _) = connect(&procs)?;
        let c = Conn {
            kitten: kitten(&procs),
            socket,
        };
        *self.conn.borrow_mut() = Some(c.clone());
        Ok(c)
    }
}

impl Backend for Kitty {
    fn name(&self) -> &'static str {
        "kitty"
    }

    fn display_name(&self) -> &'static str {
        "kitty"
    }

    fn caps(&self) -> Caps {
        Caps { splits: true }
    }

    fn installed(&self) -> bool {
        osa::app_installed("kitty.app")
    }

    fn host_pids(&self, procs: &[Proc]) -> Vec<i32> {
        procs::pids_by_exe(procs, EXE_SUFFIX)
    }

    fn read_layout(&self, procs: &[Proc]) -> Result<RawLayout> {
        let instances = connect_all(procs);
        if instances.is_empty() {
            connect(procs)?;
        }
        let mut layout = RawLayout {
            windows: Vec::new(),
        };
        for (_, ls) in instances {
            layout.windows.extend(parse_layout(&ls, procs)?.windows);
        }
        Ok(layout)
    }

    fn ensure_running(&self) -> Result<()> {
        if !self.running(&procs::process_table()?) {
            osa::launch(BUNDLE)?;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        self.conn.replace(None);
        loop {
            match self.conn() {
                Ok(_) => return Ok(()),
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }

    fn blank_windows(&self) -> Result<Vec<String>> {
        let ls = self.conn()?.remote(&["ls"], None)?;
        let os_windows: Vec<OsWindow> = serde_json::from_str(&ls)?;
        Ok(os_windows
            .iter()
            .filter(|ow| ow.tabs.len() == 1 && ow.tabs[0].windows.len() == 1)
            .map(|ow| ow.tabs[0].windows[0].id.to_string())
            .collect())
    }

    fn close_windows(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn()?;
        for id in ids {
            let _ = conn.remote(&["close-window", &format!("--match=id:{id}")], None);
        }
        Ok(())
    }

    fn restore_script(&self, window: &WindowPlan, _delay_ms: u64) -> String {
        steps(window)
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let input = s
                    .stdin
                    .as_ref()
                    .map(|t| match t.strip_suffix('\r') {
                        Some(cmd) => {
                            format!("printf '%s\\r' {} | ", shell_quote(cmd))
                        }
                        None => format!("printf %s {} | ", shell_quote(t)),
                    })
                    .unwrap_or_default();
                // `$N` stands for an id a previous step returned: left as is.
                let args: Vec<String> = s
                    .args
                    .iter()
                    .map(|a| {
                        if a.starts_with("--match=id:$") {
                            a.clone()
                        } else {
                            shell_quote(a)
                        }
                    })
                    .collect();
                format!("{input}kitten @ {}    # ${i}\n", args.join(" "))
            })
            .collect()
    }

    fn restore_window(&self, window: &WindowPlan, delay_ms: u64) -> Result<()> {
        let conn = self.conn()?;
        let mut ids: Vec<String> = Vec::new();
        for step in steps(window) {
            if step.args[0] == "launch" && !ids.is_empty() {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let args: Vec<String> = step
                .args
                .iter()
                .map(|a| match a.strip_prefix("--match=id:$") {
                    Some(n) => {
                        let id = n.parse::<usize>().ok().and_then(|n| ids.get(n));
                        format!("--match=id:{}", id.map(String::as_str).unwrap_or(""))
                    }
                    None => a.clone(),
                })
                .collect();
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = conn.remote(&refs, step.stdin.as_deref())?;
            ids.push(out.lines().last().unwrap_or("").trim().to_string());
        }
        Ok(())
    }

    fn settings(&self) -> Vec<Setting> {
        let files = config_files();
        let remote = conf::read_key(&files, REMOTE, Syntax::Space);
        let listen = conf::read_key(&files, LISTEN, Syntax::Space);
        vec![
            Setting {
                name: REMOTE,
                ok: matches!(
                    remote.as_deref(),
                    Some("yes" | "socket-only" | "socket" | "y" | "true")
                ),
                current: remote,
                wanted: "socket-only",
                why: "termrevive drives kitty through its remote control (restart kitty after the fix)",
            },
            Setting {
                name: LISTEN,
                ok: listen.as_deref().is_some_and(|l| l.starts_with("unix:")),
                current: listen,
                // kitty expands the variable: a socket in the user's own
                // temporary directory, not in the shared /tmp.
                wanted: "unix:${TMPDIR}/kitty",
                why: "the remote control needs a unix socket (restart kitty after the fix)",
            },
        ]
    }

    fn apply_setting(&self, setting: &Setting) -> Result<PathBuf> {
        conf::write_key(&config_files(), setting.name, setting.wanted, Syntax::Space)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TabPlan;

    const LS: &str = r#"[{"id":2,"is_active":true,"tabs":[
        {"id":2,"title":"~/a","is_active":false,"layout":"fat","windows":[
            {"id":2,"title":"~/a","pid":6986,"cwd":"/a","cmdline":["/bin/zsh"],"foreground_processes":[]}]},
        {"id":3,"title":"less","is_active":true,"layout":"fat","windows":[
            {"id":3,"title":"less","pid":6990,"cwd":"/tmp"},
            {"id":4,"title":"/usr","pid":6996,"cwd":"/usr"}]}]}]"#;

    #[test]
    fn parses_ls_and_turns_pids_into_ttys() {
        let procs = vec![Proc {
            pid: 6990,
            ppid: 1,
            pgid: 6990,
            tpgid: 6990,
            tty: "ttys030".into(),
            comm: "/bin/zsh".into(),
        }];
        let l = parse_layout(LS, &procs).unwrap();
        let tabs = &l.windows[0].tabs;
        assert!(!tabs[0].selected && tabs[1].selected);
        assert_eq!(tabs[1].terminals.len(), 2);
        assert_eq!(tabs[1].terminals[0].tty.as_deref(), Some("ttys030"));
        assert_eq!(tabs[1].terminals[1].tty, None);
        assert_eq!(tabs[1].terminals[1].cwd, "/usr");
    }

    #[test]
    fn plans_os_window_tabs_splits_text_and_focus() {
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
        let s = steps(&plan);
        let args: Vec<String> = s.iter().map(|s| s.args.join(" ")).collect();
        assert_eq!(
            args,
            vec![
                "launch --type=os-window --cwd=/a",
                "send-text --match=id:$0 --stdin",
                "launch --type=window --match=id:$0 --location=vsplit --cwd=/b",
                "launch --type=tab --match=id:$0 --cwd=/c",
                "send-text --match=id:$3 --stdin",
                "focus-window --match=id:$3",
            ]
        );
        assert_eq!(s[1].stdin.as_deref(), Some("claude --resume x\r"));
        assert_eq!(s[4].stdin.as_deref(), Some("make"));
    }

    #[test]
    fn the_dry_run_is_quoted_for_a_shell() {
        let plan = WindowPlan {
            tabs: vec![TabPlan {
                selected: true,
                panes: vec![Launch {
                    cwd: "/a b".into(),
                    input: Some("it's".into()),
                    run: true,
                }],
            }],
        };
        let s = Kitty::default().restore_script(&plan, 0);
        assert!(
            s.starts_with("kitten @ launch --type=os-window '--cwd=/a b'    # $0\n"),
            "{s}"
        );
        assert!(
            s.contains(r"printf '%s\r' 'it'\''s' | kitten @ send-text --match=id:$0 --stdin"),
            "{s}"
        );
    }
}
