//! Snapshots on disk: `<state>/snapshots/<timestamp>.json`, newest also copied
//! to `<state>/latest.json`, plus the small marker files used by auto mode.

use crate::model::Snapshot;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const KEEP: usize = 30;

pub fn state_dir() -> PathBuf {
    crate::paths::env_or_home("XDG_STATE_HOME", ".local/state")
        .unwrap_or_else(|| PathBuf::from("."))
        .join("termrevive")
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
    // Named after the process: a manual save and the agent's can overlap.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
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

/// Held while a restore runs, so auto-save does not record a half-built layout
/// and two restores never run at once. It is an advisory lock (`flock`) on
/// `restoring.pid`: the kernel releases it when the process ends, however it
/// ends, so an interrupted restore cannot leave it behind.
pub struct RestoreLock(fs::File);

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("restoring.pid")
}

fn try_lock(file: &fs::File) -> bool {
    // SAFETY: the descriptor stays open for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

impl RestoreLock {
    pub fn acquire() -> Result<RestoreLock> {
        Self::acquire_in(&state_dir())
    }

    fn acquire_in(dir: &Path) -> Result<RestoreLock> {
        fs::create_dir_all(dir)?;
        let path = lock_path(dir);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // An auto-save checking the lock holds it for an instant: retry briefly.
        let mut tries = 0;
        while !try_lock(&file) {
            tries += 1;
            if tries == 10 {
                bail!("another restore is running ({})", path.display());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The pid is informative; agents of termrevive ≤ 1.3.0 still read it.
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        Ok(RestoreLock(file))
    }

    /// Whether a restore is running right now.
    pub fn held() -> bool {
        Self::held_in(&state_dir())
    }

    fn held_in(dir: &Path) -> bool {
        // Taking the lock proves nobody holds it; closing the file gives it back.
        fs::File::open(lock_path(dir)).is_ok_and(|file| !try_lock(&file))
    }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        // Emptied, not removed: unlinking a lock file would let another
        // process lock a new file of the same name while this one is held.
        let _ = self.0.set_len(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_lock_is_exclusive_and_ends_with_its_holder() {
        let dir = std::env::temp_dir().join(format!("termrevive-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(!RestoreLock::held_in(&dir), "no lock file yet");

        // What an interrupted restore of termrevive ≤ 1.3.0 leaves behind: a
        // file naming a pid that is alive (here, ours). It is not a lock.
        fs::create_dir_all(&dir).unwrap();
        fs::write(lock_path(&dir), std::process::id().to_string()).unwrap();
        assert!(!RestoreLock::held_in(&dir));

        let lock = RestoreLock::acquire_in(&dir).unwrap();
        assert!(RestoreLock::held_in(&dir));
        assert!(
            RestoreLock::acquire_in(&dir).is_err(),
            "a second restore is refused"
        );
        drop(lock);
        assert!(!RestoreLock::held_in(&dir));
        assert!(RestoreLock::acquire_in(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
