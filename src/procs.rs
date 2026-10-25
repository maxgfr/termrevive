//! Process-table inspection (macOS): which shells live under a terminal emulator,
//! what runs in their foreground, and where.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub tpgid: i32,
    /// `ttys004`, or `??` when detached.
    pub tty: String,
    pub comm: String,
}

/// One shell session hosted by the terminal emulator.
#[derive(Debug, Clone, PartialEq)]
pub struct TtySession {
    pub tty: String,
    pub shell_pid: i32,
    pub shell_cwd: Option<String>,
    /// Foreground job leader, if something other than the shell is running.
    pub fg: Option<Proc>,
}

pub fn process_table() -> Result<Vec<Proc>> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,tpgid=,tty=,comm="])
        .output()
        .context("running ps")?;
    if !out.status.success() {
        bail!("ps failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_ps(text: &str) -> Vec<Proc> {
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

/// PIDs of the processes whose executable path ends with `exe_suffix`.
pub fn find_by_exe<'a>(procs: &'a [Proc], exe_suffix: &str) -> Vec<&'a Proc> {
    procs
        .iter()
        .filter(|p| p.comm.ends_with(exe_suffix))
        .collect()
}

/// Shell sessions whose `login` (or shell) is a direct child of one of `parents`.
pub fn tty_sessions(procs: &[Proc], parents: &[i32]) -> Vec<TtySession> {
    let mut sessions = Vec::new();
    for top in procs
        .iter()
        .filter(|p| parents.contains(&p.ppid) && p.tty != "??")
    {
        let shell = if basename(&top.comm) == "login" {
            match procs.iter().find(|p| p.ppid == top.pid && p.tty == top.tty) {
                Some(s) => s,
                None => continue,
            }
        } else {
            top
        };
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
        sessions.push(TtySession {
            tty: shell.tty.clone(),
            shell_pid: shell.pid,
            shell_cwd: None,
            fg,
        });
    }
    sessions.sort_by(|a, b| a.tty.cmp(&b.tty));
    sessions
}

/// Current working directories of `pids`, via one `lsof` call.
pub fn cwds(pids: &[i32]) -> HashMap<i32, String> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = Command::new("/usr/sbin/lsof")
        .args(["-a", "-d", "cwd", "-p", &list, "-Fpn"])
        .output()
    else {
        return HashMap::new();
    };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

fn parse_lsof(text: &str) -> HashMap<i32, String> {
    let mut map = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
            map.insert(p, n.to_string());
        }
    }
    map
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
        let ghostty: Vec<i32> = find_by_exe(&procs, "/ghostty")
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(ghostty, vec![8185]);
        let s = tty_sessions(&procs, &ghostty);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].shell_pid, 46087);
        assert_eq!(s[0].fg.as_ref().map(|p| p.pid), Some(46274));
        assert_eq!(s[1].shell_pid, 91580);
        assert!(s[1].fg.is_none());
    }

    #[test]
    fn parses_lsof_fields() {
        let m = parse_lsof("p35736\nfcwd\nn/Users/me\np46087\nfcwd\nn/Users/me/a b\n");
        assert_eq!(m[&46087], "/Users/me/a b");
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
    }
}
