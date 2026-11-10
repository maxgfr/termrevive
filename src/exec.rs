//! Child processes with a deadline. One that hangs (an app that stopped
//! answering Apple events, a permission prompt nobody sees) is stopped and
//! reported, instead of blocking the caller — the agent's loop — for good.

use anyhow::{Context, Result, anyhow};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Plenty for the system tools (`defaults`, `plutil`, `launchctl`, `lsof`…).
pub const QUICK: Duration = Duration::from_secs(15);

fn name(cmd: &Command) -> String {
    let program = Path::new(cmd.get_program());
    program
        .file_name()
        .unwrap_or(program.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn stop(mut child: Child, name: &str, timeout: Duration) -> anyhow::Error {
    let _ = child.kill();
    let _ = child.wait();
    anyhow!(
        "{name} did not finish within {} s and was stopped",
        timeout.as_secs_f32()
    )
}

/// Waits for `child` to end, until `deadline`.
fn wait(mut child: Child, name: &str, timeout: Duration, deadline: Instant) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(stop(child, name, timeout));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn read_all<R: Read + Send + 'static>(
    pipe: Option<R>,
    tx: mpsc::Sender<(bool, Vec<u8>)>,
    err: bool,
) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send((err, bytes));
    });
}

/// Runs `cmd` to its end with `stdin` piped in, and returns what it wrote.
/// Past `timeout` it is killed and that is the error.
pub fn output(cmd: &mut Command, stdin: Option<&str>, timeout: Duration) -> Result<Output> {
    let name = name(cmd);
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {name}"))?;
    if let (Some(mut pipe), Some(text)) = (child.stdin.take(), stdin) {
        let text = text.to_owned();
        // From a thread: a child that never reads would block a large write.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    // Both pipes are read at once, so that a child filling one is never stuck.
    let (tx, rx) = mpsc::channel();
    read_all(child.stdout.take(), tx.clone(), false);
    read_all(child.stderr.take(), tx, true);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((false, bytes)) => stdout = bytes,
            Ok((true, bytes)) => stderr = bytes,
            Err(_) => return Err(stop(child, &name, timeout)),
        }
    }
    let status = wait(child, &name, timeout, deadline)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Runs `cmd` to its end, its output discarded. Past `timeout` it is killed
/// and that is the error.
pub fn status(cmd: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    let name = name(cmd);
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {name}"))?;
    wait(child, &name, timeout, Instant::now() + timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipes_the_input_in_and_collects_both_outputs() {
        let out = output(&mut Command::new("/bin/cat"), Some("héllo\n"), QUICK).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, "héllo\n".as_bytes());

        let out = output(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            None,
            QUICK,
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(
            (out.stdout, out.stderr),
            (b"out\n".to_vec(), b"err\n".to_vec())
        );
    }

    #[test]
    fn output_larger_than_a_pipe_does_not_block() {
        // 300 KB on each side: more than the 64 KB a pipe holds.
        let script = "head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2";
        let out = output(Command::new("/bin/sh").args(["-c", script]), None, QUICK).unwrap();
        assert_eq!((out.stdout.len(), out.stderr.len()), (300_000, 300_000));
    }

    #[test]
    fn a_child_that_hangs_is_stopped_and_reported() {
        let short = Duration::from_millis(200);
        let started = Instant::now();
        let err = output(Command::new("/bin/sleep").arg("30"), None, short).unwrap_err();
        assert!(err.to_string().contains("sleep did not finish"), "{err}");
        let err = status(Command::new("/bin/sleep").arg("30"), short).unwrap_err();
        assert!(err.to_string().contains("was stopped"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        // Without input the child reads an empty stdin, not ours.
        assert!(output(&mut Command::new("/bin/cat"), None, short).is_ok());
        assert!(
            status(&mut Command::new("/usr/bin/true"), QUICK)
                .unwrap()
                .success()
        );
    }
}
