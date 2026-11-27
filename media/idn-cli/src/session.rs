//! `idn session`: the IDN Node's side in this desktop session with no window
//! ([`idn_open_protocol::node::session::Agent`]): this session's services (the IDN
//! Microphone, programs' sound, screens, webcams, the switcher and desk when set to run
//! here: spec/extensions/idn-node.md §N.14, roadmap R.19), the lists the Node's panel
//! asks for, link notices. IDN Media starts it at every log-in (it has no tray icon: the
//! UI is the embedding application's, whose tray item can embed the same agent).
//! `--autostart` adds it to a session's autostart by hand; `--no-autostart` hides it
//! there (the system's entry too).

const USAGE: &str = "\
usage: idn session [--autostart | --no-autostart]

  The IDN Node's side in this desktop session, with no window: runs this computer's
  services as the IDN Node's settings ask (its panel's \"Sound on this computer\" and
  \"Screens and cameras\"; idn node set audio-… capture-…), each an idn process kept
  running, started again when it stops, stopped when no longer wanted; tells the Node
  the programs with sound, the displays and the encoders here; says when another
  computer asks to link. IDN Media starts it at every log-in; --autostart adds it to this
  session's autostart (~/.config/autostart/idn-session.desktop), --no-autostart takes it
  out (that file then says Hidden=true, which also hides the system's
  /etc/xdg/autostart entry). One program a session runs them (this one waits while
  another does).";

fn autostart_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("autostart").join("idn-session.desktop"))
}

pub fn main() -> std::io::Result<()> {
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    if let Some(a) = args.next() {
        let file = autostart_file().ok_or_else(|| std::io::Error::other("no home folder"))?;
        match a.as_str() {
            "--autostart" => {
                let me = std::env::current_exe()?;
                std::fs::create_dir_all(file.parent().expect("a folder"))?;
                std::fs::write(
                    &file,
                    format!(
                        "[Desktop Entry]\nType=Application\nName=IDN session\nComment=The IDN Node's side in this session: sound, screens and webcams to and from IDN\nExec=\"{}\" session\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
                        me.display()
                    ),
                )?;
                println!("added {}", file.display());
                return Ok(());
            }
            "--no-autostart" => {
                // Hidden=true (not the file deleted): IDN Media's packages start it from
                // /etc/xdg/autostart/idn-session.desktop, which an entry of the same name
                // here overrides.
                std::fs::create_dir_all(file.parent().expect("a folder"))?;
                std::fs::write(
                    &file,
                    "[Desktop Entry]\nType=Application\nName=IDN session\nExec=idn-session\nNoDisplay=true\nHidden=true\nX-GNOME-Autostart-enabled=false\n",
                )?;
                println!("hidden: {} (Hidden=true)", file.display());
                return Ok(());
            }
            "-h" | "--help" => args.help(),
            other => args.fail(&format!("unexpected argument: {other}")),
        }
    }
    // One program runs this session's services: the agent waits while another does.
    let agent =
        idn_open_protocol::node::session::Agent::start(idn_open_protocol::node::session::Config {
            app: "idn session".into(),
            notice: std::sync::Arc::new(|(title, text): (String, String)| {
                println!("{title}: {text}");
                // The desktop's notifications, where there are some (Linux).
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "IDN", &title, &text])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }),
            ..Default::default()
        })?;
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
