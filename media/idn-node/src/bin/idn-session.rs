//! `idn-session`: the IDN Node's side in a desktop session, with no window and no icon
//! ([`idn_open_protocol::node::session::Agent`]): this session's services (sound, screens,
//! webcams, the switcher and desk when set to run here), the lists the Node's panel asks
//! for, notices (link requests, the site to confirm, waiting for approval). IDN Media
//! starts it at every log-in (Windows: the Run key; Linux: the session's autostart): IDN
//! Media has no UI but the Node's control panel (the UI is the
//! embedding application's, whose tray item can embed the same agent). `idn session` is
//! the same in a terminal. `--open` opens the Node's control panel in the browser.
//! One agent runs a session (the agent's own guard): a second waits while the first runs.

// No console window on Windows (started at log-on).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    // `--open`: the Node's control panel in the browser (the Start menu's and the
    // applications menu's "IDN Node"; no console window), then done.
    if std::env::args().nth(1).as_deref() == Some("--open") {
        let url = idn_open_protocol::node::Node::connect("IDN session")
            .and_then(|n| n.request(idn_open_protocol::xml::Element::new("panel_token")))
            .ok()
            .and_then(|t| t.attr("url").map(str::to_owned));
        match url {
            Some(u) => {
                idn_open_protocol::web::open_browser(&u);
            }
            None => {
                let text = "The IDN Node doesn't answer (is it running?)";
                eprintln!("idn-session: {text}");
                #[cfg(windows)]
                message_box("IDN Node", text).join().ok();
            }
        }
        return;
    }
    let agent = match idn_open_protocol::node::session::Agent::start(
        idn_open_protocol::node::session::Config {
            app: "IDN session".into(),
            notice: std::sync::Arc::new(|(title, text): (String, String)| {
                eprintln!("{title}: {text}");
                // Windows: no console (a windows-subsystem program), so a message box, on
                // a thread of its own (the agent goes on).
                #[cfg(windows)]
                drop(message_box(&title, &text));
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "IDN", &title, &text])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }),
            ..Default::default()
        },
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("idn-session: {e}");
            std::process::exit(1)
        }
    };
    // Stopped (log-off, Ctrl+C): the services stop too, removing what they made.
    let agent = std::sync::Arc::new(std::sync::Mutex::new(Some(agent)));
    idn_open_protocol::shutdown::install();
    let _stop = {
        let agent = agent.clone();
        idn_open_protocol::shutdown::register(move || {
            if let Some(a) = idn_common::sync::LockOk::lock_ok(&*agent).take() {
                a.stop();
            }
        })
    };
    loop {
        std::thread::park();
    }
}

/// A message box (OK, information) on a thread of its own; the handle joins it.
#[cfg(windows)]
fn message_box(title: &str, text: &str) -> std::thread::JoinHandle<()> {
    let (title, text) = (idn_common::win::wide(title), idn_common::win::wide(text));
    std::thread::spawn(move || {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        };
        // SAFETY: NUL-terminated strings alive for the call; no owner window.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            );
        }
    })
}
