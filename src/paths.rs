//! Where files live: a directory named by an environment variable, else one
//! under the home directory.

use std::ffi::OsString;
use std::path::PathBuf;

/// `$VAR`, else `~/<default>`. A variable set to nothing counts as unset, as
/// the XDG specification asks.
pub fn env_or_home(var: &str, default: &str) -> Option<PathBuf> {
    pick(std::env::var_os(var), dirs::home_dir(), default)
}

fn pick(value: Option<OsString>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    match value {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(home?.join(default)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let home = || Some(PathBuf::from("/h"));
        assert_eq!(
            pick(Some("/x".into()), home(), ".local/state"),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            pick(Some("".into()), home(), ".local/state"),
            Some(PathBuf::from("/h/.local/state"))
        );
        assert_eq!(
            pick(None, home(), ".config"),
            Some(PathBuf::from("/h/.config"))
        );
        assert_eq!(pick(None, None, ".config"), None);
    }
}
