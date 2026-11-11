//! Snapshots on disk: `<state>/snapshots/<timestamp>.json`, newest also copied
//! to `<state>/latest.json`, the last one of the previous boot kept as
//! `<state>/previous-boot.json`, plus the small marker files used by auto mode.
//! Snapshots hold command lines, which can carry secrets: they are written
//! readable by their owner only.

use crate::model::{Boot, Snapshot};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const KEEP: usize = 30;
/// Past this size the agent's log is moved to `agent.log.1`.
const LOG_MAX: u64 = 1 << 20;

/// `$XDG_STATE_HOME/termrevive`, else `~/.local/state/termrevive`.
pub fn state_dir() -> PathBuf {
    crate::paths::env_or_home("XDG_STATE_HOME", ".local/state")
        .unwrap_or_else(|| PathBuf::from("."))
        .join("termrevive")
}

pub fn read(path: &Path) -> Result<Snapshot> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice::<Snapshot>(&bytes)
        .map(Snapshot::normalize)
        .with_context(|| format!("parsing {}", path.display()))
}

fn create_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Writes `bytes` to `path` (mode 0600) through a temporary file, so that a
/// reader never sees half of it.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    // Named after the process: a manual save and the agent's can overlap.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)
    };
    write().with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Makes `path` readable by its owner only, if it exists.
pub fn make_private(path: &Path) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

/// The state directory: snapshots, markers, the restore lock and the logs.
pub struct Store {
    dir: PathBuf,
    /// A test's own directory, removed with the store.
    #[cfg(test)]
    scratch: bool,
}

impl Store {
    /// The user's own, [`state_dir`].
    pub fn open() -> Store {
        Store::at(state_dir())
    }

    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store {
            dir: dir.into(),
            #[cfg(test)]
            scratch: false,
        }
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.dir.join("snapshots")
    }

    pub fn latest_path(&self) -> PathBuf {
        self.dir.join("latest.json")
    }

    pub fn previous_boot_path(&self) -> PathBuf {
        self.dir.join("previous-boot.json")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("logs/agent.log")
    }

    pub fn latest(&self) -> Result<Option<Snapshot>> {
        let p = self.latest_path();
        if p.exists() {
            read(&p).map(Some)
        } else {
            Ok(None)
        }
    }

    /// The last snapshot of the boot before the one that saved last.
    pub fn previous_boot(&self) -> Result<Option<Snapshot>> {
        let p = self.previous_boot_path();
        if p.exists() {
            read(&p).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Writes `snap`, updates `latest.json` and prunes old snapshots. The
    /// first snapshot of a boot keeps the latest one of the previous boot as
    /// `previous-boot.json` before replacing it: pruning, or saves of fresh
    /// windows after a reboot, never take it away.
    pub fn write(&self, snap: &Snapshot) -> Result<PathBuf> {
        let dir = self.snapshots_dir();
        create_dir(&dir)?;
        // Directories made by termrevive ≤ 1.3.1 were readable by everyone.
        for d in [&self.dir, &dir] {
            let _ = fs::set_permissions(d, fs::Permissions::from_mode(0o700));
        }
        // Read once: the bytes kept are those of the boot compared.
        if let Ok(bytes) = fs::read(self.latest_path())
            && let Ok(latest) = serde_json::from_slice::<Snapshot>(&bytes)
            && !latest.boot().same(&snap.boot())
        {
            atomic_write(&self.previous_boot_path(), &bytes)?;
        }
        let json = serde_json::to_vec_pretty(snap)?;
        let path = dir.join(format!("{}.json", snap.taken_at.format("%Y%m%d-%H%M%S")));
        atomic_write(&path, &json)?;
        atomic_write(&self.latest_path(), &json)?;
        for (i, old) in self.list()?.into_iter().enumerate() {
            if i >= KEEP {
                let _ = fs::remove_file(old);
            } else if fs::metadata(&old).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
                make_private(&old);
            }
        }
        Ok(path)
    }

    /// How long ago `previous-boot.json` was kept: since the first save of
    /// the current boot.
    pub fn previous_boot_age(&self) -> Option<std::time::Duration> {
        let at = fs::metadata(self.previous_boot_path())
            .and_then(|m| m.modified())
            .ok()?;
        SystemTime::now().duration_since(at).ok()
    }

    /// The boot during which the user saved by hand: what is open then is
    /// what they want to keep.
    pub fn saved_by_hand(&self) -> Option<Boot> {
        let text = fs::read_to_string(self.dir.join("saved-by-hand")).ok()?;
        let mut lines = text.lines();
        let time = lines.next()?.trim().parse().ok()?;
        let session = lines.next().map(str::trim).filter(|s| !s.is_empty());
        Some(Boot {
            time,
            session: session.map(String::from),
        })
    }

    pub fn mark_saved_by_hand(&self, boot: &Boot) -> Result<()> {
        create_dir(&self.dir)?;
        let text = format!("{}\n{}\n", boot.time, boot.session.as_deref().unwrap_or(""));
        fs::write(self.dir.join("saved-by-hand"), text)?;
        Ok(())
    }

    /// Saved snapshots, newest first. They are ordered by when they were
    /// written, not by name: names are local times, which repeat for an hour
    /// when the clocks go back.
    pub fn list(&self) -> Result<Vec<PathBuf>> {
        let Ok(entries) = fs::read_dir(self.snapshots_dir()) else {
            return Ok(Vec::new());
        };
        let mut paths: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                let at = fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                (at, p)
            })
            .collect();
        paths.sort();
        paths.reverse();
        Ok(paths.into_iter().map(|(_, p)| p).collect())
    }

    /// The boot for which a restore already ran.
    pub fn restored_boot(&self) -> Option<Boot> {
        let time = fs::read_to_string(self.dir.join("restored-boot"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        // In a file of its own: agents of termrevive ≤ 1.3.1 read the first.
        let session = fs::read_to_string(self.dir.join("restored-boot-session"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Some(Boot { time, session })
    }

    pub fn mark_restored(&self, boot: &Boot) -> Result<()> {
        create_dir(&self.dir)?;
        let session = self.dir.join("restored-boot-session");
        match &boot.session {
            Some(s) => fs::write(&session, s)?,
            None => {
                let _ = fs::remove_file(&session);
            }
        }
        fs::write(self.dir.join("restored-boot"), boot.time.to_string())?;
        Ok(())
    }

    /// Moves the agent's log to `agent.log.1` once it passes 1 MB, and keeps
    /// both readable by their owner only. The log is copied, then emptied in
    /// place: launchd keeps it open for appending, so it goes on in the same
    /// file.
    pub fn rotate_log(&self) {
        let log = self.log_path();
        make_private(&log);
        if fs::metadata(&log).is_ok_and(|m| m.len() > LOG_MAX) {
            let old = log.with_extension("log.1");
            if fs::copy(&log, &old).is_ok() {
                make_private(&old);
                let _ = fs::OpenOptions::new()
                    .write(true)
                    .open(&log)
                    .and_then(|f| f.set_len(0));
            }
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("restoring.pid")
    }

    /// Takes the restore lock, or fails when another restore holds it.
    pub fn lock(&self) -> Result<RestoreLock> {
        create_dir(&self.dir)?;
        let path = self.lock_path();
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
    pub fn restoring(&self) -> bool {
        // Taking the lock proves nobody holds it; closing the file gives it back.
        fs::File::open(self.lock_path()).is_ok_and(|file| !try_lock(&file))
    }
}

/// Held while a restore runs, so auto-save does not record a half-built layout
/// and two restores never run at once. It is an advisory lock (`flock`) on
/// `restoring.pid`: the kernel releases it when the process ends, however it
/// ends, so an interrupted restore cannot leave it behind.
pub struct RestoreLock(fs::File);

fn try_lock(file: &fs::File) -> bool {
    // SAFETY: the descriptor stays open for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        // Emptied, not removed: unlinking a lock file would let another
        // process lock a new file of the same name while this one is held.
        let _ = self.0.set_len(0);
    }
}

/// A store in a new directory of its own, for tests.
#[cfg(test)]
pub fn scratch(name: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("termrevive-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Store { dir, scratch: true }
}

#[cfg(test)]
impl Drop for Store {
    fn drop(&mut self) {
        if self.scratch {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// A snapshot with nothing in it, for tests.
#[cfg(test)]
pub fn empty_snapshot(boot: &Boot) -> Snapshot {
    Snapshot {
        version: crate::model::SNAPSHOT_VERSION,
        taken_at: chrono::Local::now(),
        boot_time: boot.time,
        boot_session: boot.session.clone(),
        backend: String::new(),
        windows: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn boot(time: i64, session: &str) -> Boot {
        Boot {
            time,
            session: Some(session.into()),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn restore_lock_is_exclusive_and_ends_with_its_holder() {
        let store = scratch("lock");
        assert!(!store.restoring(), "no lock file yet");

        // What an interrupted restore of termrevive ≤ 1.3.0 leaves behind: a
        // file naming a pid that is alive (here, ours). It is not a lock.
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(store.lock_path(), std::process::id().to_string()).unwrap();
        assert!(!store.restoring());

        let lock = store.lock().unwrap();
        assert!(store.restoring());
        assert!(store.lock().is_err(), "a second restore is refused");
        drop(lock);
        assert!(!store.restoring());
        assert!(store.lock().is_ok());
    }

    #[test]
    fn the_last_snapshot_of_the_previous_boot_is_kept() {
        let store = scratch("previous-boot");
        let mut before = empty_snapshot(&boot(1, "A"));
        before.taken_at -= chrono::Duration::days(1);
        store.write(&before).unwrap();
        store.write(&empty_snapshot(&boot(1, "A"))).unwrap();
        assert!(store.previous_boot().unwrap().is_none(), "same boot");

        // After the reboot: more saves than are kept.
        for i in 0..KEEP + 5 {
            let mut s = empty_snapshot(&boot(2, "B"));
            s.taken_at += chrono::Duration::seconds(i as i64);
            store.write(&s).unwrap();
        }
        let kept = store.previous_boot().unwrap().unwrap();
        assert_eq!(kept.boot(), boot(1, "A"));
        assert_eq!(store.list().unwrap().len(), KEEP);
        assert_eq!(store.latest().unwrap().unwrap().boot(), boot(2, "B"));
        for p in [
            store.latest_path(),
            store.previous_boot_path(),
            store.list().unwrap()[0].clone(),
        ] {
            assert_eq!(mode(&p), 0o600, "{}", p.display());
        }
        assert_eq!(mode(&store.dir), 0o700);
        assert!(store.previous_boot_age().unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn files_of_older_versions_become_private() {
        let store = scratch("private");
        let dir = store.snapshots_dir();
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("20260101-000000.json");
        fs::write(&old, "{}").unwrap();
        for p in [&store.dir, &dir] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
        store.write(&empty_snapshot(&boot(1, "A"))).unwrap();
        assert_eq!(
            (mode(&store.dir), mode(&dir), mode(&old)),
            (0o700, 0o700, 0o600)
        );
    }

    #[test]
    fn a_save_by_hand_is_recorded_with_its_boot() {
        let store = scratch("by-hand");
        assert_eq!(store.saved_by_hand(), None);
        store.mark_saved_by_hand(&boot(3, "C")).unwrap();
        assert_eq!(store.saved_by_hand(), Some(boot(3, "C")));
        let no_session = Boot {
            time: 4,
            session: None,
        };
        store.mark_saved_by_hand(&no_session).unwrap();
        assert_eq!(store.saved_by_hand(), Some(no_session));
    }

    #[test]
    fn snapshots_are_listed_by_when_they_were_written() {
        let store = scratch("order");
        let dir = store.snapshots_dir();
        fs::create_dir_all(&dir).unwrap();
        // 02:59 then, the clocks gone back, 02:01.
        let now = SystemTime::now();
        for (name, ago) in [("20261025-025900.json", 120), ("20261025-020100.json", 0)] {
            let p = dir.join(name);
            fs::write(&p, "{}").unwrap();
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(now - Duration::from_secs(ago))
                .unwrap();
        }
        let names: Vec<String> = store
            .list()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["20261025-020100.json", "20261025-025900.json"]);
    }

    #[test]
    fn the_restored_boot_keeps_its_session_apart() {
        let store = scratch("restored");
        assert_eq!(store.restored_boot(), None);
        store.mark_restored(&boot(7, "U")).unwrap();
        assert_eq!(store.restored_boot(), Some(boot(7, "U")));
        // What termrevive ≤ 1.3.1 reads: the boot time alone.
        assert_eq!(
            fs::read_to_string(store.dir.join("restored-boot")).unwrap(),
            "7"
        );
        store
            .mark_restored(&Boot {
                time: 8,
                session: None,
            })
            .unwrap();
        assert_eq!(store.restored_boot().unwrap().session, None);
    }

    #[test]
    fn the_log_is_capped_and_private() {
        let store = scratch("log");
        let log = store.log_path();
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "short\n").unwrap();
        store.rotate_log();
        assert_eq!(fs::read_to_string(&log).unwrap(), "short\n");
        assert_eq!(mode(&log), 0o600);

        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(&vec![b'x'; LOG_MAX as usize]).unwrap();
        store.rotate_log();
        let old = log.with_extension("log.1");
        assert_eq!(fs::metadata(&old).unwrap().len(), LOG_MAX + 6);
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
        // A writer that appends goes on at the start of the emptied file.
        f.write_all(b"next\n").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "next\n");
        assert_eq!(mode(&old), 0o600);
    }
}
