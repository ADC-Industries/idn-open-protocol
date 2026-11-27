//! Running a system tool with a time limit: a tool that hangs (a sound server that
//! stopped answering `pactl`, a stuck D-Bus behind `busctl`) is killed and reported,
//! instead of holding the thread that asked for ever.

use crate::sync::LockOk;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// How long the output is still read once the tool has ended: a program it started may
/// hold the pipes open (its output is then what came so far).
const DRAIN: Duration = Duration::from_secs(2);

/// [`Command::output`] and [`Command::status`] with a time limit.
pub trait RunWithin {
    /// Runs it (no input; its output and errors taken), waiting at most `limit`: past it,
    /// the program and the programs it started are killed and the answer is a `TimedOut`
    /// error.
    fn output_within(&mut self, limit: Duration) -> io::Result<Output>;
    /// As [`output_within`](Self::output_within), only its exit status; its output and
    /// errors go where this program's go (a terminal, the service's log).
    fn status_within(&mut self, limit: Duration) -> io::Result<ExitStatus>;
}

impl RunWithin for Command {
    fn output_within(&mut self, limit: Duration) -> io::Result<Output> {
        let name = self.get_program().to_string_lossy().into_owned();
        own_group(self);
        let mut child = self
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Read on threads of their own (a full pipe would otherwise stop the program),
        // into buffers that can be taken before the pipe closes.
        let (done_tx, done) = mpsc::channel::<()>();
        let pipe = |r: Option<Box<dyn Read + Send>>| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let (b, tx) = (buf.clone(), done_tx.clone());
            std::thread::spawn(move || {
                if let Some(mut r) = r {
                    let mut chunk = [0u8; 8192];
                    while let Ok(n) = r.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        b.lock_ok().extend_from_slice(&chunk[..n]);
                    }
                }
                let _ = tx.send(());
            });
            buf
        };
        let out = pipe(
            child
                .stdout
                .take()
                .map(|r| Box::new(r) as Box<dyn Read + Send>),
        );
        let err = pipe(
            child
                .stderr
                .take()
                .map(|r| Box::new(r) as Box<dyn Read + Send>),
        );
        drop(done_tx);
        let status = wait_within(&mut child, limit, &name)?;
        let end = Instant::now() + DRAIN;
        for _ in 0..2 {
            if done
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .is_err()
            {
                break;
            }
        }
        let take = |b: &Arc<Mutex<Vec<u8>>>| std::mem::take(&mut *b.lock_ok());
        Ok(Output {
            status,
            stdout: take(&out),
            stderr: take(&err),
        })
    }

    fn status_within(&mut self, limit: Duration) -> io::Result<ExitStatus> {
        let name = self.get_program().to_string_lossy().into_owned();
        own_group(self);
        let mut child = self.stdin(Stdio::null()).spawn()?;
        wait_within(&mut child, limit, &name)
    }
}

/// On Unix, the tool in a process group of its own, so a timeout stops what it started
/// too (`update-grub`'s `grub-mkconfig`). For callers that run it themselves: with
/// [`kill_all`].
pub fn own_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Stops the tool and what it started: its process group on Unix (made by [`own_group`]),
/// its process tree on Windows.
pub fn kill_all(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill takes no pointers; -pid is the group `own_group` made (pid > 0).
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(mut t) = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end && matches!(t.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = t.kill();
            let _ = t.wait();
        }
    }
    let _ = child.kill();
}

fn wait_within(
    child: &mut std::process::Child,
    limit: Duration,
    name: &str,
) -> io::Result<ExitStatus> {
    let end = Instant::now() + limit;
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        if Instant::now() >= end {
            kill_all(child);
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{name} didn't finish within {} s: stopped",
                    limit.as_secs_f32()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_hung_tool_is_stopped() {
        let t = Instant::now();
        let e = Command::new("sleep")
            .arg("10")
            .output_within(Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(2));
        let o = Command::new("sh")
            .args(["-c", "echo out; echo err >&2"])
            .output_within(Duration::from_secs(5))
            .unwrap();
        assert!(o.status.success());
        assert_eq!(
            (o.stdout.as_slice(), o.stderr.as_slice()),
            (&b"out\n"[..], &b"err\n"[..])
        );
    }

    /// A timeout stops what the tool started too, and a program left holding its output
    /// doesn't hold the answer.
    #[test]
    fn its_children_go_with_it_and_dont_hold_the_answer() {
        let mark = std::env::temp_dir().join(format!("idn-cmd-{}", std::process::id()));
        let _ = std::fs::remove_file(&mark);
        let script = format!("(sleep 1; touch {}) & sleep 10", mark.display());
        let e = Command::new("sh")
            .args(["-c", &script])
            .output_within(Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!mark.exists(), "the tool's child outlived the timeout");
        // The tool ends at once; a child it left keeps its output open for 10 s.
        let t = Instant::now();
        let o = Command::new("sh")
            .args(["-c", "echo hi; sleep 10 &"])
            .output_within(Duration::from_secs(30))
            .unwrap();
        assert_eq!(o.stdout, b"hi\n");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }
}
