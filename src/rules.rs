//! Turns a saved pane into "what to type, and where" using configurable rules.

use crate::model::Pane;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = r#"# termrevive configuration. Rules are tried top to bottom, your rules first,
# then the built-in ones (unless builtin_rules = false). The first rule whose
# `match` lists the foreground program and whose template can be filled wins.
#
# Template variables:
#   {argv}               the saved command line, shell-quoted
#   {cwd}                the pane's working directory
#   {claude_session_id}  {claude_cwd}  {claude_name}   (Claude Code panes only)
#
# run = true types the command and presses Enter; run = false only types it.

builtin_rules = true

[restore]
delay_ms = 300   # pause between two tabs/splits

# [[rule]]
# match = ["npm", "pnpm"]
# command = "{argv}"
# run = false

[default]        # any other program
command = "{argv}"
run = false
"#;

const BUILTIN_RULES: &str = r#"
[[rule]]
match = ["claude"]
command = "claude --resume {claude_session_id}"
cwd = "{claude_cwd}"
run = true

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

    fn all_rules(&self) -> impl Iterator<Item = Rule> + '_ {
        let builtins = if self.builtin_rules {
            toml::from_str::<Builtins>(BUILTIN_RULES)
                .expect("built-in rules parse")
                .rule
        } else {
            Vec::new()
        };
        self.rules.iter().cloned().chain(builtins)
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
        for rule in self.all_rules() {
            if !rule.matches.contains(&program.name) {
                continue;
            }
            let Some(cmd) = render(&rule.command, pane) else {
                continue;
            };
            let cwd = rule
                .cwd
                .as_deref()
                .and_then(|c| render(c, pane))
                .filter(|c| Path::new(c).is_dir())
                .unwrap_or(base.cwd);
            return Launch {
                cwd,
                input: (!cmd.is_empty()).then_some(cmd),
                run: rule.run,
            };
        }
        let cmd = render(&self.default.command, pane).filter(|c| !c.is_empty());
        Launch {
            input: cmd,
            run: self.default.run,
            ..base
        }
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
    let claude = program.and_then(|p| p.claude.as_ref());
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
            "claude_session_id" => claude?.session_id.clone(),
            "claude_cwd" => claude?.cwd.clone(),
            "claude_name" => shell_quote(claude?.name.as_deref()?),
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
    use crate::model::{ClaudeSession, Program};

    fn pane(name: &str, argv: &[&str], claude: Option<ClaudeSession>) -> Pane {
        Pane {
            title: String::new(),
            cwd: "/".into(),
            program: Some(Program {
                name: name.into(),
                argv: argv.iter().map(|s| s.to_string()).collect(),
                claude,
            }),
        }
    }

    #[test]
    fn claude_resumes_in_its_session_cwd() {
        let tmp = std::env::temp_dir().display().to_string();
        let p = pane(
            "claude",
            &["claude"],
            Some(ClaudeSession {
                session_id: "abc".into(),
                cwd: tmp.clone(),
                name: None,
            }),
        );
        let l = Config::default().launch(&p);
        assert_eq!(l.input.as_deref(), Some("claude --resume abc"));
        assert_eq!(l.cwd, tmp);
        assert!(l.run);
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
