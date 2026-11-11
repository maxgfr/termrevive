//! Turns a saved pane into "what to type, and where" using configurable rules.

use crate::model::Pane;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = r#"# termrevive configuration. Rules are tried top to bottom: your rules first,
# then, for an agent CLI whose session was found (claude, codex, vibe, opencode,
# kilo), its harness' own resume command, then the built-in rules (unless
# builtin_rules = false), then [default]. The first rule whose `match` lists the
# program (or the harness) and whose template can be filled wins.
#
# Template variables:
#   {argv}               the saved command line, shell-quoted
#   {cwd}                the pane's working directory
#   {session_id}  {session_cwd}  {session_name}   (agent sessions only)
#   {claude_session_id}  {claude_cwd}  {claude_name}   (same, older names)
# A literal brace is written {{ or }}: command = "awk '{{print $1}}'".
# `termrevive doctor` points out a template that cannot be filled.
#
# A rule without `cwd` runs in the pane's directory, or for an agent session
# in the session's own directory (its project or worktree).
#
# run = true types the command and presses Enter; run = false only types it.

builtin_rules = true

[restore]
delay_ms = 300   # pause between two tabs/splits

# [[rule]]
# match = ["npm", "pnpm"]
# command = "{argv}"
# run = false

# [[rule]]                       # resume Codex sessions in full-auto mode
# match = "codex"
# command = "codex resume {session_id} --full-auto"
# cwd = "{session_cwd}"

[default]        # any other program
command = "{argv}"
run = false
"#;

const BUILTIN_RULES: &str = r#"
[[rule]]
match = ["nvim", "vim", "vi", "hx", "emacs", "nano", "ssh", "mosh", "htop", "btop", "top", "less", "man", "lazygit", "tig", "k9s", "yazi"]
command = "{argv}"
run = true
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub builtin_rules: bool,
    pub restore: RestoreConfig,
    #[serde(rename = "rule")]
    pub rules: Vec<Rule>,
    pub default: DefaultRule,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RestoreConfig {
    pub delay_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    #[serde(rename = "match")]
    pub matches: OneOrMany,
    pub command: String,
    pub cwd: Option<String>,
    #[serde(default = "yes")]
    pub run: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DefaultRule {
    pub command: String,
    pub run: bool,
}

#[derive(Deserialize)]
struct Builtins {
    rule: Vec<Rule>,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            builtin_rules: true,
            restore: RestoreConfig::default(),
            rules: Vec::new(),
            default: DefaultRule::default(),
        }
    }
}

impl Default for RestoreConfig {
    fn default() -> Self {
        RestoreConfig { delay_ms: 300 }
    }
}

impl Default for DefaultRule {
    fn default() -> Self {
        DefaultRule {
            command: "{argv}".into(),
            run: false,
        }
    }
}

impl OneOrMany {
    fn contains(&self, name: &str) -> bool {
        match self {
            OneOrMany::One(s) => s == name,
            OneOrMany::Many(v) => v.iter().any(|s| s == name),
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    Some(crate::backend::conf::xdg_dir("termrevive")?.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Config> {
        let Some(path) = config_path().filter(|p| p.exists()) else {
            return Ok(Config::default());
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Templates that can never be filled: a rule using one is skipped.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            let at = format!("[[rule]] #{}", i + 1);
            out.extend(check(&rule.command).map(|e| format!("{at} command: {e}")));
            if let Some(cwd) = &rule.cwd {
                out.extend(check(cwd).map(|e| format!("{at} cwd: {e}")));
            }
        }
        out.extend(check(&self.default.command).map(|e| format!("[default] command: {e}")));
        out
    }

    fn builtins(&self) -> Vec<Rule> {
        if self.builtin_rules {
            toml::from_str::<Builtins>(BUILTIN_RULES)
                .expect("built-in rules parse")
                .rule
        } else {
            Vec::new()
        }
    }

    /// How to bring `pane` back.
    pub fn launch(&self, pane: &Pane) -> Launch {
        let home = || {
            crate::paths::home()
                .map(|h| h.display().to_string())
                .unwrap_or_default()
        };
        let cwd_or_home = |c: &str| {
            if !c.is_empty() && Path::new(c).is_dir() {
                c.to_string()
            } else {
                home()
            }
        };
        let base = Launch {
            cwd: cwd_or_home(&pane.cwd),
            input: None,
            run: false,
        };
        let Some(program) = &pane.program else {
            return base;
        };
        let session_cwd = || {
            program
                .agent
                .as_ref()
                .map(|a| a.cwd.clone())
                .filter(|c| Path::new(c).is_dir())
                .unwrap_or_else(|| base.cwd.clone())
        };
        // Your rules first. One without a `cwd` that matches an agent session
        // runs where the session belongs (its project or worktree), which is
        // where the agent's resume command finds it.
        let rule_cwd = if program.agent.is_some() {
            session_cwd()
        } else {
            base.cwd.clone()
        };
        for rule in &self.rules {
            if let Some(l) = self.apply(rule, pane, &rule_cwd) {
                return l;
            }
        }
        // Then the harness' own way to resume the session it found.
        if let Some(agent) = &program.agent
            && let Some(h) = crate::harness::by_name(&agent.harness)
        {
            return Launch {
                cwd: session_cwd(),
                input: Some(h.resume_command(agent)),
                run: true,
            };
        }
        for rule in self.builtins() {
            if let Some(mut l) = self.apply(&rule, pane, &base.cwd) {
                // A shell on the remote host comes back on its own; a command
                // run there is only typed, like any unknown command.
                l.run &= !remote_command(&program.argv);
                return l;
            }
        }
        let cmd = render(&self.default.command, pane, true).filter(|c| !c.is_empty());
        Launch {
            input: cmd,
            run: self.default.run,
            ..base
        }
    }

    /// `rule` applied to `pane`, if it matches the program (or its harness)
    /// and its templates can be filled.
    fn apply(&self, rule: &Rule, pane: &Pane, fallback_cwd: &str) -> Option<Launch> {
        let program = pane.program.as_ref()?;
        let harness = program.agent.as_ref().map(|a| a.harness.as_str());
        if !rule.matches.contains(&program.name)
            && !harness.is_some_and(|h| rule.matches.contains(h))
        {
            return None;
        }
        let cmd = render(&rule.command, pane, true)?;
        let cwd = rule
            .cwd
            .as_deref()
            .and_then(|c| render(c, pane, false))
            .filter(|c| Path::new(c).is_dir())
            .unwrap_or_else(|| fallback_cwd.to_string());
        Some(Launch {
            cwd,
            input: (!cmd.is_empty()).then_some(cmd),
            run: rule.run,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub cwd: String,
    /// Text typed into the new shell, if any.
    pub input: Option<String>,
    /// Whether Enter is pressed after `input`.
    pub run: bool,
}

/// The variables a template may use.
const VARIABLES: &[&str] = &[
    "argv",
    "cwd",
    "session_id",
    "session_cwd",
    "session_name",
    "claude_session_id",
    "claude_cwd",
    "claude_name",
];

/// A piece of a template.
#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Text(&'a str),
    Var(&'a str),
}

/// Splits a template into text and `{var}`s; `{{` and `}}` are a literal
/// brace. Fails on a brace that opens or closes nothing.
fn parse(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        if at > 0 {
            out.push(Piece::Text(&rest[..at]));
        }
        let brace = &rest[at..at + 1];
        if rest[at + 1..].starts_with(brace) {
            out.push(Piece::Text(brace));
            rest = &rest[at + 2..];
        } else if brace == "}" {
            return Err(format!("a lone `}}` in `{template}` (write `}}}}`)"));
        } else {
            let end = rest[at..]
                .find('}')
                .ok_or_else(|| format!("an unclosed `{{` in `{template}` (write `{{{{`)"))?;
            out.push(Piece::Var(&rest[at + 1..at + end]));
            rest = &rest[at + end + 1..];
        }
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    Ok(out)
}

/// What is wrong with `template`, if anything.
fn check(template: &str) -> Option<String> {
    match parse(template) {
        Err(e) => Some(e),
        Ok(pieces) => pieces.iter().find_map(|p| match p {
            Piece::Var(v) if !VARIABLES.contains(v) => Some(format!(
                "unknown variable `{{{v}}}` in `{template}` (a literal brace is `{{{{` or `}}}}`)"
            )),
            _ => None,
        }),
    }
}

/// Fills `{var}` placeholders; `None` if one is unknown or unavailable. With
/// `quote` the result is a command line and every value is shell-quoted;
/// without, it is a path and values are left as they are.
fn render(template: &str, pane: &Pane, quote: bool) -> Option<String> {
    let program = pane.program.as_ref();
    let agent = program.and_then(|p| p.agent.as_ref());
    let value = |s: &str| {
        if quote { shell_quote(s) } else { s.to_string() }
    };
    let mut out = String::new();
    for piece in parse(template).ok()? {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Var(v) => out.push_str(&match v {
                "argv" => program?
                    .argv
                    .iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
                "cwd" => value(&pane.cwd),
                "session_id" | "claude_session_id" => value(&agent?.session_id),
                "session_cwd" | "claude_cwd" => value(&agent?.cwd),
                "session_name" | "claude_name" => value(agent?.name.as_deref()?),
                _ => return None,
            }),
        }
    }
    Some(out)
}

/// Whether an `ssh` or `mosh` command line runs a command on the remote host
/// (`ssh host 'rm -rf build'`) rather than opening a shell there.
fn remote_command(argv: &[String]) -> bool {
    let Some(name) = argv.first().map(|a| crate::procs::basename(a)) else {
        return false;
    };
    // Options that take a value, as `-p 22` or `-p22`.
    let (short, long): (&str, &[&str]) = match name {
        "ssh" => ("BbcDEeFIiJLlmOoPpQRSWw", &[]),
        "mosh" => (
            "p",
            &[
                "--client",
                "--server",
                "--ssh",
                "--port",
                "--predict",
                "--bind-server",
                "--family",
                "--experimental-remote-ip",
            ],
        ),
        _ => return false,
    };
    let mut args = argv[1..].iter();
    let mut positional = 0;
    while let Some(a) = args.next() {
        if a == "--" {
            positional += args.len();
            break;
        }
        if a.starts_with("--") {
            if long.contains(&a.as_str()) {
                args.next();
            }
        } else if let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            // Flags can be grouped (`-vp 22`); the first that takes a value
            // ends the group, its value being the rest or the next argument.
            if let Some(i) = flags.find(|c| short.contains(c))
                && i + 1 == flags.len()
            {
                args.next();
            }
        } else {
            positional += 1;
        }
    }
    // The destination, then the command.
    positional > 1
}

pub fn shell_quote(s: &str) -> String {
    // `~` is left out: it expands at the start of a word and, in bash, after
    // a `=` or `:`. A leading `=` is zsh's `=command` expansion.
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !s.is_empty() && s.chars().all(safe) && !s.starts_with('=') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentSession, Program};

    fn pane(name: &str, argv: &[&str], agent: Option<AgentSession>) -> Pane {
        Pane {
            title: String::new(),
            cwd: "/".into(),
            program: Some(Program {
                name: name.into(),
                argv: argv.iter().map(|s| s.to_string()).collect(),
                agent,
                ..Default::default()
            }),
        }
    }

    fn session(harness: &str, id: &str, cwd: &str) -> Option<AgentSession> {
        Some(AgentSession {
            harness: harness.into(),
            session_id: id.into(),
            cwd: cwd.into(),
            name: None,
        })
    }

    #[test]
    fn claude_resumes_in_its_session_cwd() {
        let tmp = std::env::temp_dir().display().to_string();
        let p = pane("claude", &["claude"], session("claude", "abc", &tmp));
        let l = Config::default().launch(&p);
        assert_eq!(l.input.as_deref(), Some("claude --resume abc"));
        assert_eq!(l.cwd, tmp);
        assert!(l.run);
    }

    #[test]
    fn every_harness_resumes_its_own_way() {
        let cases = [
            ("codex", "codex resume 019a"),
            ("vibe", "vibe --resume 019a"),
            ("opencode", "opencode --session 019a"),
            ("kilo", "kilo --session 019a"),
        ];
        for (h, want) in cases {
            let l = Config::default().launch(&pane(h, &[h], session(h, "019a", "/nope")));
            assert_eq!(l.input.as_deref(), Some(want));
            assert!(l.run);
            assert_eq!(l.cwd, "/", "missing session dir falls back to the pane's");
        }
    }

    #[test]
    fn user_rules_can_override_a_harness_by_name() {
        let cfg: Config = toml::from_str(
            "[[rule]]\nmatch = \"codex\"\ncommand = \"codex resume {session_id} --full-auto\"\n",
        )
        .unwrap();
        // The program is a wrapper; the harness name still matches.
        let l = cfg.launch(&pane(
            "codexfeine",
            &["codexfeine"],
            session("codex", "x y", "/"),
        ));
        assert_eq!(l.input.as_deref(), Some("codex resume 'x y' --full-auto"));
        let legacy: Config = toml::from_str(
            "[[rule]]\nmatch = \"claude\"\ncommand = \"claude -r {claude_session_id}\"\n",
        )
        .unwrap();
        let l = legacy.launch(&pane("claude", &["claude"], session("claude", "abc", "/")));
        assert_eq!(l.input.as_deref(), Some("claude -r abc"));
    }

    #[test]
    fn paths_are_quoted_in_commands_and_raw_in_cwd() {
        let dir = std::env::temp_dir().join(format!("termrevive rules {}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let d = dir.display().to_string();
        let cfg: Config = toml::from_str(
            "[[rule]]\nmatch = \"claude\"\ncommand = \"cd {session_cwd} && claude -r {session_id}\"\ncwd = \"{cwd}/sub\"\n",
        )
        .unwrap();
        let mut p = pane("claude", &["claude"], session("claude", "abc", &d));
        p.cwd = d.clone();
        let l = cfg.launch(&p);
        assert_eq!(l.input, Some(format!("cd '{d}' && claude -r abc")));
        assert_eq!(l.cwd, format!("{d}/sub"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_without_session_falls_back_to_default() {
        let l = Config::default().launch(&pane("claude", &["claude", "-c"], None));
        assert_eq!(l.input.as_deref(), Some("claude -c"));
        assert!(!l.run);
    }

    #[test]
    fn known_programs_rerun_with_quoted_args() {
        let l = Config::default().launch(&pane("nvim", &["nvim", "my file's.txt"], None));
        assert_eq!(l.input.as_deref(), Some(r"nvim 'my file'\''s.txt'"));
        assert!(l.run);
    }

    #[test]
    fn quotes_what_a_shell_would_expand() {
        for plain in ["make", "--flag=v", "a/b.c", "user@host:22", "x,y", "+%s"] {
            assert_eq!(shell_quote(plain), plain);
        }
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("=ls"), "'=ls'", "zsh: =ls is /bin/ls");
        assert_eq!(shell_quote("~/x"), "'~/x'");
        assert_eq!(shell_quote("dir=~"), "'dir=~'", "bash expands it");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn unknown_programs_are_only_typed() {
        let l = Config::default().launch(&pane("make", &["make", "deploy"], None));
        assert_eq!(l.input.as_deref(), Some("make deploy"));
        assert!(!l.run);
    }

    #[test]
    fn user_rules_override_builtins() {
        let cfg: Config = toml::from_str(
            "[[rule]]\nmatch = \"nvim\"\ncommand = \"nvim -S\"\n[default]\ncommand = \"\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.launch(&pane("nvim", &["nvim"], None)).input.as_deref(),
            Some("nvim -S")
        );
        assert_eq!(cfg.launch(&pane("make", &["make"], None)).input, None);
        assert_eq!(cfg.restore.delay_ms, 300);
    }

    #[test]
    fn idle_shell_and_missing_cwd_go_home() {
        let p = Pane {
            title: String::new(),
            cwd: "/definitely/not/here".into(),
            program: None,
        };
        let l = Config::default().launch(&p);
        assert_eq!(l.input, None);
        assert_eq!(l.cwd, crate::paths::home().unwrap().display().to_string());
    }

    #[test]
    fn literal_braces_and_unknown_variables() {
        let p = pane("awk", &["awk"], None);
        assert_eq!(
            render("awk '{{print $1}}' {argv}", &p, true).as_deref(),
            Some("awk '{print $1}' awk")
        );
        assert_eq!(render("awk '{print $1}'", &p, true), None);
        assert_eq!(check("cd {session_cwd} && {argv} {{x}}"), None);
        assert!(
            check("awk '{print $1}'")
                .unwrap()
                .contains("unknown variable `{print $1}`")
        );
        assert!(check("{argv").unwrap().contains("unclosed"));
        assert!(check("x }").unwrap().contains("lone"));

        let cfg: Config = toml::from_str(
            "[[rule]]\nmatch = \"x\"\ncommand = \"{argv}\"\n\
             [[rule]]\nmatch = \"y\"\ncommand = \"{nope}\"\ncwd = \"{cwd\"\n\
             [default]\ncommand = \"{argv} {sesion_id}\"\n",
        )
        .unwrap();
        let problems = cfg.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].starts_with("[[rule]] #2 command: unknown variable `{nope}`"));
        assert!(problems[1].starts_with("[[rule]] #2 cwd: an unclosed"));
        assert!(problems[2].starts_with("[default] command: unknown variable `{sesion_id}`"));
        assert!(Config::default().problems().is_empty());
        let shipped: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(shipped.problems().is_empty());
    }

    #[test]
    fn a_rule_on_an_agent_runs_in_the_session_directory() {
        let dir = std::env::temp_dir().join(format!("termrevive worktree {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let cfg: Config =
            toml::from_str("[[rule]]\nmatch = \"claude\"\ncommand = \"claude -r {session_id}\"\n")
                .unwrap();
        // The pane's shell is in the main checkout, the session in a worktree.
        let l = cfg.launch(&pane("claude", &["claude"], session("claude", "abc", &d)));
        assert_eq!(
            (l.input.as_deref(), l.cwd.as_str()),
            (Some("claude -r abc"), d.as_str())
        );
        // Its own `cwd` still wins; a program that is no agent stays put.
        let cfg: Config = toml::from_str(
            "[[rule]]\nmatch = [\"claude\", \"make\"]\ncommand = \"{argv}\"\ncwd = \"/\"\n",
        )
        .unwrap();
        let l = cfg.launch(&pane("claude", &["claude"], session("claude", "abc", &d)));
        assert_eq!(l.cwd, "/");
        let l = Config::default().launch(&Pane {
            cwd: "/".into(),
            ..pane("make", &["make"], None)
        });
        assert_eq!(l.cwd, "/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_with_a_remote_command_is_only_typed() {
        let run = |argv: &str| {
            let argv: Vec<&str> = argv.split(' ').collect();
            Config::default().launch(&pane(argv[0], &argv, None)).run
        };
        for shell in [
            "ssh host",
            "ssh -p 2222 user@host",
            "ssh -p2222 -L 8080:localhost:80 host",
            "ssh -vA -i key host",
            "ssh -o ServerAliveInterval=30 host",
            "mosh host",
            "mosh --ssh=ssh -p 22 host",
            "mosh -p 60001 host",
        ] {
            assert!(run(shell), "{shell}");
        }
        for command in [
            "ssh host rm -rf build",
            "ssh -t host htop",
            "ssh -p 22 host -- ls",
            "mosh host -- tmux attach",
            "mosh --ssh ssh host top",
        ] {
            assert!(!run(command), "{command}");
        }
        // A rule of yours is taken as it is.
        let cfg: Config =
            toml::from_str("[[rule]]\nmatch = \"ssh\"\ncommand = \"{argv}\"\nrun = true\n")
                .unwrap();
        assert!(cfg.launch(&pane("ssh", &["ssh", "host", "ls"], None)).run);
    }

    #[test]
    fn default_config_file_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(cfg.builtin_rules && cfg.rules.is_empty());
    }
}
