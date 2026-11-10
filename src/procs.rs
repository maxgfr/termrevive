//! Process-table inspection (macOS): which shells live under a terminal emulator,
//! what runs in their foreground, and where. Everything is asked from the
//! kernel (libproc, sysctl): no `ps`, which costs a process per save.

use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Foreground process group of its terminal; 0 without one.
    pub tpgid: i32,
    /// `ttys004`, or `??`: detached, or a process of another user (`login`,
    /// which root owns), whose terminal the kernel does not tell.
    pub tty: String,
    /// Path of the executable; its short name when the path cannot be read.
    pub comm: String,
}

/// One shell session hosted by the terminal emulator.
#[derive(Debug, Clone, PartialEq)]
pub struct TtySession {
    pub tty: String,
    pub shell_pid: i32,
    /// Foreground job leader, if something other than the shell is running.
    pub fg: Option<Proc>,
}

unsafe extern "C" {
    /// `<stdlib.h>`: the name of a device, written into `buf`.
    fn devname_r(
        dev: libc::dev_t,
        kind: libc::mode_t,
        buf: *mut libc::c_char,
        len: libc::c_int,
    ) -> *mut libc::c_char;
}

/// `proc_pidinfo` for a flavor with a fixed-size answer; `None` when the
/// process is gone or the kernel refuses (another user's process).
///
/// # Safety
/// `T` must be the C struct that `flavor` fills.
unsafe fn pidinfo<T>(pid: i32, flavor: libc::c_int) -> Option<T> {
    // SAFETY: these structs are plain data, valid when zeroed, and the kernel
    // writes at most `size` bytes into `info`.
    let mut info: T = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<T>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, (&raw mut info).cast(), size) };
    (n == size).then_some(info)
}

/// The text of a NUL-terminated C array.
fn c_str(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn exe_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most the length it is given.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// `ttys004` for a terminal's device number, as `ps` prints it.
fn tty_name(dev: u32) -> String {
    let mut buf = [0 as libc::c_char; 64];
    // SAFETY: devname_r writes a NUL-terminated name of at most `len` bytes.
    let found = unsafe {
        devname_r(
            dev as libc::dev_t,
            libc::S_IFCHR,
            buf.as_mut_ptr(),
            buf.len() as libc::c_int,
        )
    };
    let name = c_str(&buf);
    if dev == u32::MAX || found.is_null() || name.is_empty() {
        "??".into()
    } else {
        name
    }
}

fn proc_of(pid: i32, ttys: &mut HashMap<u32, String>) -> Option<Proc> {
    // SAFETY: proc_bsdinfo is what PROC_PIDTBSDINFO fills.
    if let Some(i) = unsafe { pidinfo::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) } {
        return Some(Proc {
            pid,
            ppid: i.pbi_ppid as i32,
            pgid: i.pbi_pgid as i32,
            tpgid: i.e_tpgid as i32,
            tty: ttys
                .entry(i.e_tdev)
                .or_insert_with(|| tty_name(i.e_tdev))
                .clone(),
            comm: exe_path(pid).unwrap_or_else(|| c_str(&i.pbi_comm)),
        });
    }
    // Another user's process: the kernel only tells its lineage and its name.
    // SAFETY: proc_bsdshortinfo is what PROC_PIDT_SHORTBSDINFO fills.
    let i = unsafe { pidinfo::<libc::proc_bsdshortinfo>(pid, libc::PROC_PIDT_SHORTBSDINFO) }?;
    Some(Proc {
        pid,
        ppid: i.pbsi_ppid as i32,
        pgid: i.pbsi_pgid as i32,
        tpgid: 0,
        tty: "??".into(),
        comm: exe_path(pid).unwrap_or_else(|| c_str(&i.pbsi_comm)),
    })
}

/// Every process, lowest pid first.
pub fn process_table() -> Result<Vec<Proc>> {
    // SAFETY: a null buffer asks how many processes there are.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        bail!("could not list the processes");
    }
    // Room for those that start in between.
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: at most `bytes` bytes are written into `pids`.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if count <= 0 {
        bail!("could not list the processes");
    }
    pids.truncate(count as usize);
    pids.sort_unstable();
    let mut ttys = HashMap::new();
    Ok(pids
        .into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| proc_of(pid, &mut ttys))
        .collect())
}

/// A table in the format of `ps -axo pid=,ppid=,pgid=,tpgid=,tty=,comm=`,
/// which the fixtures use.
#[cfg(test)]
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line.trim_start();
            let mut fields = [0i32; 4];
            for f in &mut fields {
                let (tok, r) = rest.split_once(char::is_whitespace)?;
                *f = tok.parse().ok()?;
                rest = r.trim_start();
            }
            let (tty, comm) = rest.split_once(char::is_whitespace)?;
            Some(Proc {
                pid: fields[0],
                ppid: fields[1],
                pgid: fields[2],
                tpgid: fields[3],
                tty: tty.to_string(),
                comm: comm.trim().to_string(),
            })
        })
        .collect()
}

pub fn basename(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_prefix('-').unwrap_or(name)
}

/// The processes whose executable path ends with `exe_suffix`.
pub fn find_by_exe<'a>(procs: &'a [Proc], exe_suffix: &str) -> Vec<&'a Proc> {
    procs
        .iter()
        .filter(|p| p.comm.ends_with(exe_suffix))
        .collect()
}

/// Their pids: what a backend's `host_pids` returns.
pub fn pids_by_exe(procs: &[Proc], exe_suffix: &str) -> Vec<i32> {
    find_by_exe(procs, exe_suffix)
        .iter()
        .map(|p| p.pid)
        .collect()
}

/// Shell sessions whose `login` (or shell) is a direct child of one of `parents`.
pub fn tty_sessions(procs: &[Proc], parents: &[i32]) -> Vec<TtySession> {
    let mut sessions: Vec<TtySession> = procs
        .iter()
        .filter(|p| parents.contains(&p.ppid))
        .filter_map(|top| session_from(procs, top))
        .collect();
    sessions.sort_by(|a, b| a.tty.cmp(&b.tty));
    sessions
}

/// The shell session on `tty` (`ttys004`): the topmost process there (the
/// one whose parent is not on the tty), past `login`.
pub fn session_on_tty(procs: &[Proc], tty: &str) -> Option<TtySession> {
    let on_tty: Vec<&Proc> = procs.iter().filter(|p| p.tty == tty).collect();
    let top = on_tty
        .iter()
        .filter(|p| !on_tty.iter().any(|q| q.pid == p.ppid))
        .min_by_key(|p| p.pid)?;
    session_from(procs, top)
}

fn session_from(procs: &[Proc], top: &Proc) -> Option<TtySession> {
    // `login` belongs to root, so its own terminal is not known: the shell is
    // its child that has one.
    let shell = if basename(&top.comm) == "login" {
        procs.iter().find(|p| p.ppid == top.pid && p.tty != "??")?
    } else {
        top
    };
    if shell.tty == "??" {
        return None;
    }
    let fg = (shell.tpgid > 0 && shell.tpgid != shell.pgid)
        .then(|| {
            procs
                .iter()
                .find(|p| p.pid == shell.tpgid)
                .or_else(|| {
                    procs
                        .iter()
                        .filter(|p| p.pgid == shell.tpgid)
                        .min_by_key(|p| p.pid)
                })
                .cloned()
        })
        .flatten();
    Some(TtySession {
        tty: shell.tty.clone(),
        shell_pid: shell.pid,
        fg,
    })
}

/// Current working directory of `pid`; `None` when the process is gone or
/// belongs to another user.
pub fn cwd(pid: i32) -> Option<String> {
    // SAFETY: proc_vnodepathinfo is what PROC_PIDVNODEPATHINFO fills.
    let info: libc::proc_vnodepathinfo = unsafe { pidinfo(pid, libc::PROC_PIDVNODEPATHINFO) }?;
    let path = c_str(info.pvi_cdir.vip_path.as_flattened());
    (!path.is_empty()).then_some(path)
}

/// Current working directories of `pids`; those that cannot be read are left out.
pub fn cwds(pids: &[i32]) -> HashMap<i32, String> {
    pids.iter()
        .filter_map(|&pid| Some((pid, cwd(pid)?)))
        .collect()
}

/// Exact argv of `pid` (KERN_PROCARGS2), so arguments with spaces survive.
pub fn argv(pid: i32) -> Option<Vec<String>> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: mib and the output buffer are valid for the sizes passed.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: as above; the kernel writes at most `size` bytes.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    parse_procargs(&buf)
}

fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    let rest = &rest[rest.iter().position(|&b| b == 0)?..]; // skip exec path
    let rest = &rest[rest.iter().position(|&b| b != 0)?..]; // skip padding
    let args: Vec<String> = rest
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (args.len() == argc && argc > 0).then_some(args)
}

/// Seconds since the epoch at which `pid` started.
pub fn start_time(pid: i32) -> Option<i64> {
    // SAFETY: proc_bsdinfo is what PROC_PIDTBSDINFO fills.
    let info: libc::proc_bsdinfo = unsafe { pidinfo(pid, libc::PROC_PIDTBSDINFO) }?;
    Some(info.pbi_start_tvsec as i64)
}

/// Seconds since the epoch at which the machine booted.
pub fn boot_time() -> i64 {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: `tv` is a valid timeval and `size` matches it.
    unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut tv).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        );
    }
    tv.tv_sec
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const PS: &str = "\
 8185     1  8185     0 ??       /Applications/Ghostty.app/Contents/MacOS/ghostty
46086  8185 46086 46274 ttys000  /usr/bin/login
46087 46086 46087 46274 ttys000  -/bin/zsh
46274 46087 46274 46274 ttys000  claude
46277 46274 46274 46274 ttys000  caffeinate
91579  8185 91579 91580 ttys008  /usr/bin/login
91580 91579 91580 91580 ttys008  -/bin/zsh
  700     1   700     0 ??       /Applications/Some App.app/Contents/MacOS/Some App
";

    #[test]
    fn parses_ps_with_spaces_in_comm() {
        let procs = parse_ps(PS);
        assert_eq!(procs.len(), 8);
        assert_eq!(
            procs[7].comm,
            "/Applications/Some App.app/Contents/MacOS/Some App"
        );
        assert_eq!(procs[2].tty, "ttys000");
    }

    #[test]
    fn finds_shells_and_foreground_jobs() {
        let procs = parse_ps(PS);
        let ghostty = pids_by_exe(&procs, "/ghostty");
        assert_eq!(ghostty, vec![8185]);
        let s = tty_sessions(&procs, &ghostty);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].shell_pid, 46087);
        assert_eq!(s[0].fg.as_ref().map(|p| p.pid), Some(46274));
        assert_eq!(s[1].shell_pid, 91580);
        assert!(s[1].fg.is_none());
    }

    #[test]
    fn finds_the_shell_on_a_tty() {
        let procs = parse_ps(PS);
        let s = session_on_tty(&procs, "ttys000").unwrap();
        assert_eq!(s.shell_pid, 46087);
        assert_eq!(s.fg.map(|p| p.pid), Some(46274));
        assert!(session_on_tty(&procs, "ttys009").is_none());
    }

    /// What `login` looks like from the kernel: root owns it, so no terminal.
    #[test]
    fn finds_the_shell_under_a_login_whose_terminal_is_unknown() {
        let mut procs = parse_ps(PS);
        for p in procs.iter_mut().filter(|p| p.comm == "/usr/bin/login") {
            p.tty = "??".into();
            p.tpgid = 0;
        }
        let s = tty_sessions(&procs, &[8185]);
        assert_eq!(
            s.iter().map(|s| s.shell_pid).collect::<Vec<_>>(),
            vec![46087, 91580]
        );
        assert_eq!(s[0].fg.as_ref().map(|p| p.pid), Some(46274));
        let s = session_on_tty(&procs, "ttys008").unwrap();
        assert_eq!((s.shell_pid, s.fg), (91580, None));
        // A child of the emulator that has no terminal is no session.
        assert!(tty_sessions(&procs, &[1]).is_empty());
    }

    #[test]
    fn the_process_table_agrees_with_ps() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let table = process_table().unwrap();
        let ps = Command::new("/bin/ps")
            .args(["-o", "pid=,ppid=,pgid=,tpgid=,tty=,comm="])
            .args(["-p", &child.id().to_string()])
            .output()
            .unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let want = parse_ps(&String::from_utf8_lossy(&ps.stdout)).remove(0);
        let got = table.iter().find(|p| p.pid == want.pid).unwrap();
        assert_eq!(
            (got.ppid, got.pgid, got.tpgid, &got.tty),
            (want.ppid, want.pgid, want.tpgid, &want.tty)
        );
        assert_eq!(got.ppid, std::process::id() as i32);
        assert_eq!(got.comm, "/bin/sleep");
        // launchd is root's: listed all the same, with its path.
        let launchd = table.iter().find(|p| p.pid == 1).unwrap();
        assert_eq!((launchd.ppid, launchd.comm.as_str()), (0, "/sbin/launchd"));
        assert!(table.windows(2).all(|w| w[0].pid < w[1].pid));
    }

    #[test]
    fn reads_a_working_directory_with_spaces_and_accents() {
        let dir = std::env::temp_dir().join(format!("termrevive été {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // i32::MAX is no process: it is left out rather than failing the lot.
        let got = cwds(&[pid, i32::MAX]);
        let _ = child.kill();
        let _ = child.wait();
        let want = dir.canonicalize().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got.get(&pid).map(String::as_str), want.to_str());
    }

    #[test]
    fn parses_procargs2() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/nvim\0\0\0nvim\0my file.txt\0HOME=/x\0");
        assert_eq!(
            parse_procargs(&buf),
            Some(vec!["nvim".into(), "my file.txt".into()])
        );
    }

    #[test]
    fn own_argv_is_readable() {
        assert!(argv(std::process::id() as i32).is_some_and(|a| !a.is_empty()));
        assert!(boot_time() > 0);
        let me = start_time(std::process::id() as i32).unwrap();
        assert!(me >= boot_time() && me <= chrono::Local::now().timestamp());
    }
}
