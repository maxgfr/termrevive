//! Line-based `key value` / `key = value` config files (Ghostty, kitty…),
//! read and edited without touching the lines termrevive does not own. The
//! files they include are followed: Ghostty's `config-file`, kitty's `include`.

use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Syntax {
    /// `key = value` (Ghostty).
    Equals,
    /// `key value` (kitty).
    Space,
}

fn split(line: &str, syntax: Syntax) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = match syntax {
        Syntax::Equals => line.split_once('=')?,
        Syntax::Space => line.split_once(char::is_whitespace).unwrap_or((line, "")),
    };
    Some((k.trim(), v.trim()))
}

impl Syntax {
    /// The directive that includes another file.
    fn include(self) -> &'static str {
        match self {
            Syntax::Equals => "config-file",
            Syntax::Space => "include",
        }
    }
}

/// `"x"` → `x`.
fn unquote(v: &str) -> &str {
    v.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v)
}

/// `${VAR}` and `$VAR` replaced by their value; unknown ones are left as
/// they are.
fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, len) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (&after[..end], end)
        };
        match std::env::var(name).ok().filter(|_| !name.is_empty()) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[at..at + 1 + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// The file an include directive in `from` names, if any.
fn included(value: &str, from: &Path, syntax: Syntax) -> Option<PathBuf> {
    let value = match syntax {
        // `?file`: no error if it is missing.
        Syntax::Equals => unquote(value.strip_prefix('?').unwrap_or(value)).to_string(),
        Syntax::Space => expand_vars(value),
    };
    if value.is_empty() {
        return None;
    }
    let path = match value.strip_prefix("~/") {
        Some(rest) => crate::paths::home()?.join(rest),
        None => PathBuf::from(value),
    };
    Some(if path.is_relative() {
        from.parent()?.join(path)
    } else {
        path
    })
}

/// Every assignment of `key` in `files` and in the files they include, in the
/// order the terminal applies them, as (file, value). kitty reads an included
/// file where it is included; Ghostty reads them after every other file, in
/// the order they were named.
fn assignments(files: &[PathBuf], key: &str, syntax: Syntax) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<PathBuf> = files.iter().cloned().collect();
    while let Some(file) = queue.pop_front() {
        let later = visit(&file, key, syntax, &mut seen, &mut out);
        queue.extend(later);
    }
    out
}

/// Reads `file` into `out`; returns the files it includes that are read later.
fn visit(
    file: &Path,
    key: &str,
    syntax: Syntax,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<(PathBuf, String)>,
) -> Vec<PathBuf> {
    // A file included twice, or a cycle, is read once.
    let id = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let mut later = Vec::new();
    if seen.len() > 64 || !seen.insert(id) {
        return later;
    }
    let Ok(text) = fs::read_to_string(file) else {
        return later;
    };
    for l in text.lines() {
        let Some((k, v)) = split(l, syntax) else {
            continue;
        };
        if k == key {
            let v = match syntax {
                Syntax::Equals => unquote(v),
                Syntax::Space => v,
            };
            out.push((file.to_path_buf(), v.to_string()));
        } else if k == syntax.include()
            && let Some(inc) = included(v, file, syntax)
        {
            match syntax {
                Syntax::Space => {
                    let more = visit(&inc, key, syntax, seen, out);
                    later.extend(more);
                }
                Syntax::Equals => later.push(inc),
            }
        }
    }
    later
}

fn line(key: &str, value: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Equals => format!("{key} = {value}"),
        Syntax::Space => format!("{key} {value}"),
    }
}

/// Value of `key` across `files` (in load order) and the files they include,
/// the last assignment winning.
pub fn read_key(files: &[PathBuf], key: &str, syntax: Syntax) -> Option<String> {
    assignments(files, key, syntax).pop().map(|(_, v)| v)
}

/// `text` with every `key` line set to `value`, or one appended.
pub fn set_key(text: &str, key: &str, value: &str, syntax: Syntax) -> String {
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| match split(l, syntax) {
            Some((k, _)) if k == key => {
                found = true;
                line(key, value, syntax)
            }
            _ => l.to_string(),
        })
        .collect();
    if !found {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("# Set by termrevive, which restores windows and programs itself.".into());
        out.push(line(key, value, syntax));
    }
    out.join("\n") + "\n"
}

/// Sets `key` in the file that decides it: the one with the winning
/// assignment (an included file, maybe), else the first existing file, else a
/// new `files[0]`. Returns the file written.
pub fn write_key(files: &[PathBuf], key: &str, value: &str, syntax: Syntax) -> Result<PathBuf> {
    let target = assignments(files, key, syntax)
        .pop()
        .map(|(f, _)| f)
        .or_else(|| files.iter().find(|f| f.exists()).cloned())
        .or_else(|| files.first().cloned())
        .context("no config file location")?;
    let text = fs::read_to_string(&target).unwrap_or_default();
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&target, set_key(&text, key, value, syntax))
        .with_context(|| format!("writing {}", target.display()))?;
    Ok(target)
}

/// `$XDG_CONFIG_HOME/<app>` or `~/.config/<app>`.
pub fn xdg_dir(app: &str) -> Option<PathBuf> {
    Some(crate::paths::env_or_home("XDG_CONFIG_HOME", ".config")?.join(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "window-save-state";

    #[test]
    fn rewrites_or_appends_equals() {
        assert_eq!(
            set_key(
                "a = 1\nwindow-save-state = always\n# window-save-state = x\n",
                KEY,
                "never",
                Syntax::Equals
            ),
            "a = 1\nwindow-save-state = never\n# window-save-state = x\n"
        );
        assert_eq!(
            set_key("a = 1\n", KEY, "never", Syntax::Equals),
            "a = 1\n\n# Set by termrevive, which restores windows and programs itself.\nwindow-save-state = never\n"
        );
        assert!(set_key("", KEY, "never", Syntax::Equals).starts_with("# Set by termrevive"));
    }

    #[test]
    fn rewrites_or_appends_space() {
        assert_eq!(
            set_key(
                "font_size 13\nallow_remote_control no\n#allow_remote_control yes\n",
                "allow_remote_control",
                "socket-only",
                Syntax::Space
            ),
            "font_size 13\nallow_remote_control socket-only\n#allow_remote_control yes\n"
        );
        assert!(
            set_key("", "listen_on", "unix:/tmp/kitty", Syntax::Space)
                .ends_with("listen_on unix:/tmp/kitty\n")
        );
    }

    #[test]
    fn follows_included_files_the_way_each_terminal_does() {
        let dir = std::env::temp_dir().join(format!("termrevive-include-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        let write = |name: &str, text: &str| fs::write(dir.join(name), text).unwrap();

        // Ghostty: an included file is read after the one including it, so
        // it wins over what follows the directive; `?` and quotes allowed.
        write(
            "config",
            "config-file = ?\"sub/theme\"\nconfig-file = ?missing\nwindow-save-state = always\n",
        );
        write(
            "sub/theme",
            "window-save-state = \"never\"\nconfig-file = ../config\n",
        );
        let files = [dir.join("config")];
        assert_eq!(
            read_key(&files, KEY, Syntax::Equals).as_deref(),
            Some("never")
        );
        // The fix goes where it decides: the included file.
        write("sub/theme", "window-save-state = always\n");
        assert_eq!(
            write_key(&files, KEY, "never", Syntax::Equals).unwrap(),
            dir.join("sub/theme")
        );
        assert_eq!(
            read_key(&files, KEY, Syntax::Equals).as_deref(),
            Some("never")
        );

        // kitty: read where it is included; variables expanded.
        write(
            "kitty.conf",
            "allow_remote_control no\ninclude sub/a.conf\nlisten_on unix:/x\n",
        );
        write(
            "sub/a.conf",
            "allow_remote_control yes\nlisten_on unix:/y\ninclude ${HOME}/../nowhere.conf\n",
        );
        let files = [dir.join("kitty.conf")];
        let remote = "allow_remote_control";
        assert_eq!(
            read_key(&files, remote, Syntax::Space).as_deref(),
            Some("yes")
        );
        assert_eq!(
            read_key(&files, "listen_on", Syntax::Space).as_deref(),
            Some("unix:/x")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_variables_like_kitty() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_vars("${HOME}/a"), format!("{home}/a"));
        assert_eq!(expand_vars("$HOME/a"), format!("{home}/a"));
        assert_eq!(
            expand_vars("${TERMREVIVE_UNSET_VAR}/a $ b"),
            "${TERMREVIVE_UNSET_VAR}/a $ b"
        );
        assert_eq!(expand_vars("${unclosed"), "${unclosed");
    }

    #[test]
    fn writes_the_deciding_file() {
        let dir = std::env::temp_dir().join(format!("termrevive-conf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let files = [dir.join("a/config"), dir.join("b/config")];
        // Nothing exists: a new first file.
        assert_eq!(
            write_key(&files, KEY, "never", Syntax::Equals).unwrap(),
            files[0]
        );
        assert_eq!(
            read_key(&files, KEY, Syntax::Equals).as_deref(),
            Some("never")
        );
        // A later file overrides it: that one gets rewritten.
        fs::create_dir_all(files[1].parent().unwrap()).unwrap();
        fs::write(&files[1], "font-size = 13\nwindow-save-state = always\n").unwrap();
        assert_eq!(
            read_key(&files, KEY, Syntax::Equals).as_deref(),
            Some("always")
        );
        assert_eq!(
            write_key(&files, KEY, "never", Syntax::Equals).unwrap(),
            files[1]
        );
        assert_eq!(
            read_key(&files, KEY, Syntax::Equals).as_deref(),
            Some("never")
        );
        assert!(
            fs::read_to_string(&files[1])
                .unwrap()
                .starts_with("font-size = 13\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
