//! Snapshots on disk: `<state>/snapshots/<timestamp>.json`, newest also copied
//! to `<state>/latest.json`, plus the small marker files used by auto mode.

use crate::model::Snapshot;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const KEEP: usize = 30;

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("termrevive")
}

fn snapshots_dir() -> PathBuf {
    state_dir().join("snapshots")
}

pub fn latest_path() -> PathBuf {
    state_dir().join("latest.json")
}

pub fn read(path: &Path) -> Result<Snapshot> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice::<Snapshot>(&bytes)
        .map(Snapshot::normalize)
        .with_context(|| format!("parsing {}", path.display()))
}

pub fn latest() -> Result<Option<Snapshot>> {
    let p = latest_path();
    if p.exists() {
        read(&p).map(Some)
    } else {
        Ok(None)
    }
}

/// Writes `snap`, updates `latest.json` and prunes old snapshots.
pub fn write(snap: &Snapshot) -> Result<PathBuf> {
    let dir = snapshots_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = serde_json::to_vec_pretty(snap)?;
    let path = dir.join(format!("{}.json", snap.taken_at.format("%Y%m%d-%H%M%S")));
    atomic_write(&path, &json)?;
    atomic_write(&latest_path(), &json)?;
    for old in list()?.into_iter().skip(KEEP) {
        let _ = fs::remove_file(old);
    }
    Ok(path)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Saved snapshots, newest first.
pub fn list() -> Result<Vec<PathBuf>> {
    let Ok(entries) = fs::read_dir(snapshots_dir()) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths.reverse();
    Ok(paths)
}

/// Boot (kern.boottime) for which auto-restore already ran.
pub fn restored_boot() -> Option<i64> {
    fs::read_to_string(state_dir().join("restored-boot"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn mark_restored(boot: i64) -> Result<()> {
    fs::create_dir_all(state_dir())?;
    fs::write(state_dir().join("restored-boot"), boot.to_string())?;
    Ok(())
}

/// Held while a restore runs so auto-save does not record a half-built layout.
pub struct RestoreLock(PathBuf);

impl RestoreLock {
    pub fn acquire() -> Result<RestoreLock> {
        fs::create_dir_all(state_dir())?;
        let path = state_dir().join("restoring.pid");
        fs::write(&path, std::process::id().to_string())?;
        Ok(RestoreLock(path))
    }

    /// Whether another live process is restoring right now.
    pub fn held() -> bool {
        let Ok(pid) = fs::read_to_string(state_dir().join("restoring.pid")) else {
            return false;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
