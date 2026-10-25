//! Line-based `key value` / `key = value` config files (Ghostty, kitty…),
//! read and edited without touching the lines termrevive does not own.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

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

fn line(key: &str, value: &str, syntax: Syntax) -> String {
    match syntax {
        Syntax::Equals => format!("{key} = {value}"),
        Syntax::Space => format!("{key} {value}"),
    }
}

/// Value of `key` across `files` (in load order), the last assignment winning.
pub fn read_key(files: &[PathBuf], key: &str, syntax: Syntax) -> Option<String> {
    let mut value = None;
    for text in files.iter().filter_map(|f| fs::read_to_string(f).ok()) {
        for l in text.lines() {
            if let Some((k, v)) = split(l, syntax)
                && k == key
            {
                value = Some(v.to_string());
            }
        }
    }
    value
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

/// Sets `key` in the file that decides it: the last one assigning it, else the
/// first existing file, else a new `files[0]`. Returns the file written.
pub fn write_key(files: &[PathBuf], key: &str, value: &str, syntax: Syntax) -> Result<PathBuf> {
    let assigns = |f: &&PathBuf| {
        fs::read_to_string(f).is_ok_and(|t| {
            t.lines()
                .any(|l| split(l, syntax).is_some_and(|(k, _)| k == key))
        })
    };
    let target = files
        .iter()
        .rev()
        .find(assigns)
        .or_else(|| files.iter().find(|f| f.exists()))
        .or(files.first())
        .context("no config file location")?
        .clone();
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
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()?.join(".config"),
    };
    Some(base.join(app))
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
