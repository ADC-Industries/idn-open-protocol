//! The desktop's clipboard as text, for remote control (roadmap R.25): a viewer reads its
//! own before a paste, the controlled machine is given it. Windows: the Win32 clipboard;
//! Linux: `wl-copy`/`wl-paste` on Wayland (wl-clipboard), `xclip` on X11 (the desktop's
//! own tools, so nothing is linked against a display library).

use std::io;
#[cfg(not(windows))]
use std::io::{Read, Write};
#[cfg(not(windows))]
use std::process::{Command, Stdio};

/// The clipboard's text, if it has some.
pub fn get() -> Option<String> {
    #[cfg(windows)]
    {
        crate::win_clipboard_get()
    }
    #[cfg(not(windows))]
    {
        let (cmd, args): (&str, &[&str]) = if wayland() {
            ("wl-paste", &["--no-newline", "--type", "text/plain"])
        } else {
            ("xclip", &["-selection", "clipboard", "-o"])
        };
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Read on a thread, waited for at most 2 s: a frozen clipboard owner held the
        // injector, and with it all of the session's input.
        let stdout = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let ok = stdout
                .take(crate::MAX_CLIPBOARD as u64 + 1)
                .read_to_end(&mut out)
                .is_ok();
            let _ = tx.send(ok.then_some(out));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(2));
        let _ = child.kill();
        let _ = child.wait();
        let out = got.ok().flatten()?;
        (!out.is_empty() && out.len() <= crate::MAX_CLIPBOARD)
            .then(|| String::from_utf8_lossy(&out).into_owned())
    }
}

/// Puts `text` on the clipboard.
pub fn set(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        crate::win_clipboard_set(text)
    }
    #[cfg(not(windows))]
    {
        let (cmd, args): (&str, &[&str]) = if wayland() {
            ("wl-copy", &["--type", "text/plain"])
        } else {
            ("xclip", &["-selection", "clipboard", "-i"])
        };
        // (Both stay running to serve the selection, as clipboard owners must: not waited
        // for.)
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "{cmd}: {e} (install {})",
                        if cmd == "wl-copy" {
                            "wl-clipboard"
                        } else {
                            "xclip"
                        }
                    ),
                )
            })?;
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no pipe"))?
            .write_all(text.as_bytes())?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[cfg(not(windows))]
fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}
