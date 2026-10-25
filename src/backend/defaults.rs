//! macOS user defaults (`defaults read/write`), for apps configured that way.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// `defaults read <domain> <key>`; `None` when unset.
pub fn read(domain: &str, key: &str) -> Option<String> {
    let out = Command::new("/usr/bin/defaults")
        .args(["read", domain, key])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `defaults write <domain> <key> -bool <value>`.
pub fn write_bool(domain: &str, key: &str, value: bool) -> Result<()> {
    let status = Command::new("/usr/bin/defaults")
        .args([
            "write",
            domain,
            key,
            "-bool",
            if value { "true" } else { "false" },
        ])
        .status()
        .context("running defaults")?;
    if !status.success() {
        bail!("`defaults write {domain} {key}` failed");
    }
    Ok(())
}

/// Path shown to the user for a defaults domain.
pub fn plist_path(domain: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Preferences")
        .join(format!("{domain}.plist"))
}

/// Cocoa's own window restoration, which every AppKit app honors: with this set,
/// the app ignores its saved windows at launch, so they don't come back on top
/// of a restore.
pub const IGNORE_STATE: &str = "ApplePersistenceIgnoreState";
