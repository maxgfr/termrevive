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
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()?.join(".config"),
    };
    Some(base.join("termrevive").join("config.toml"))
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
            dirs::home_dir()
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
        // Your rules first.
        for rule in &self.rules {
            if let Some(l) = self.apply(rule, pane, &base.cwd) {
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
            if let Some(l) = self.apply(&rule, pane, &base.cwd) {
                return l;
            }
        }
        let cmd = render(&self.default.command, pane).filter(|c| !c.is_empty());
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
        let cmd = render(&rule.command, pane)?;
        let cwd = rule
            .cwd
            .as_deref()
            .and_then(|c| render(c, pane))
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

/// Fills `{var}` placeholders; `None` if one is unknown or unavailable.
fn render(template: &str, pane: &Pane) -> Option<String> {
    let program = pane.program.as_ref();
    let agent = program.and_then(|p| p.agent.as_ref());
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}')? + start;
        let value = match &rest[start + 1..end] {
            "argv" => program?
                .argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
            "cwd" => shell_quote(&pane.cwd),
            "session_id" | "claude_session_id" => shell_quote(&agent?.session_id),
            "session_cwd" | "claude_cwd" => agent?.cwd.clone(),
            "session_name" | "claude_name" => shell_quote(agent?.name.as_deref()?),
            _ => return None,
        };
        out.push_str(&value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-~".contains(c);
    if !s.is_empty() && s.chars().all(safe) && !s.starts_with('~') {
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
        assert_eq!(l.cwd, dirs::home_dir().unwrap().display().to_string());
    }

    #[test]
    fn default_config_file_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(cfg.builtin_rules && cfg.rules.is_empty());
    }
}
