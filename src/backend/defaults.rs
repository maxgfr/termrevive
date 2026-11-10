//! macOS user defaults (`defaults read/write`), for apps configured that way.

use super::Setting;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

/// Cocoa's own window restoration, which every AppKit app honors: with this set,
/// the app ignores its saved windows at launch, so they don't come back on top
/// of a restore.
pub const IGNORE_STATE: &str = "ApplePersistenceIgnoreState";

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

/// `defaults write <domain> <key> -bool true`; returns the file shown to the
/// user for that domain.
pub fn enable(domain: &str, key: &str) -> Result<PathBuf> {
    let status = Command::new("/usr/bin/defaults")
        .args(["write", domain, key, "-bool", "true"])
        .status()
        .context("running defaults")?;
    if !status.success() {
        bail!("`defaults write {domain} {key}` failed");
    }
    Ok(dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Preferences")
        .join(format!("{domain}.plist")))
}

/// The [`IGNORE_STATE`] setting of the app `domain`, as `doctor` shows it.
pub fn ignore_state(domain: &str, why: &'static str) -> Setting {
    let current = read(domain, IGNORE_STATE);
    Setting {
        name: IGNORE_STATE,
        ok: current.as_deref() == Some("1"),
        current,
        wanted: "1",
        why,
    }
}
