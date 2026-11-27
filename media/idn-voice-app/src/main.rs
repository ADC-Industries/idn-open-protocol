//! idn-voice: IDN Voice on the desktop (spec/extensions/idn-voice.md §V.11). The Voice web
//! client the Site Manager serves, in a window of its own instead of a browser tab, and
//! what a browser tab can't do:
//! - a talk key or foot pedal that works from any program (Windows), held to talk where
//!   the user talked last;
//! - a call brings the window forward, even from the notification area;
//! - it keeps running in the notification area when its window closes, and starts at
//!   log-in if the user wants (in the background);
//! - the microphone allowed for the Voice server without asking each time, and the Site
//!   Manager's own (self-signed) certificate trusted once the user has compared its
//!   fingerprint.
//!
//! Windows: WebView2 (the Edge engine that comes with Windows 10 and 11), its loader DLL
//! next to the program. Linux: WebKitGTK, no global talk key yet (Wayland has no way for
//! a program to see keys meant for others).
//!
//! `--hidden`: start in the background (the log-in start); `--setup`: the server page.

// Elsewhere `main` only says where it runs: the pages and messages below go unused there.
#![cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]

use idn_common::sync::LockOk as _;

mod settings;

#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(windows)]
fn main() {
    idn_open_protocol::diag::init("idn-voice");
    win::main()
}

#[cfg(target_os = "linux")]
fn main() {
    idn_open_protocol::diag::init("idn-voice");
    linux::main()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("idn-voice runs on Windows and Linux; elsewhere open the Voice page in a browser");
    std::process::exit(1);
}

/// Put into every Voice page: a call (the page's `idn-voice-ring` event) goes to the app,
/// which brings its window forward.
pub const BRIDGE: &str = r#"(() => {
  if (window.__idnVoiceApp) return;
  window.__idnVoiceApp = true;
  const host = (m) => {
    const s = JSON.stringify(m);
    try {
      if (window.chrome && window.chrome.webview) window.chrome.webview.postMessage(s);
      else window.webkit.messageHandlers.idnVoice.postMessage(s);
    } catch (e) {}
  };
  window.addEventListener('idn-voice-ring', (e) => host({ t: 'ring', from: (e.detail && e.detail.from) || '' }));
})();"#;

/// The talk key, given to the page: down or up (Windows: Linux has no global talk key).
#[cfg(windows)]
pub fn key_script(down: bool) -> String {
    format!(
        "window.dispatchEvent(new CustomEvent('idn-voice-key', {{ detail: {{ down: {down} }} }}));"
    )
}

/// The setup page shown last's own word: its messages carry it, so no other page (a
/// server's, one loaded before the setup page took its place) can pass for it (bug round
/// E, E5-7).
static SETUP_NONCE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The app's own page: the Voice server, the talk key, the start at log-in; `error` said
/// above it (couldn't connect, a certificate not trusted).
pub fn setup_page(s: &settings::Settings, error: &str) -> String {
    let nonce = idn_open_protocol::random::token_hex(16);
    *SETUP_NONCE.lock_ok() = nonce.clone();
    let state = serde_json::json!({
        "nonce": nonce,
        "server": s.server,
        "key_name": s.key_name,
        "pass_key": s.pass_key,
        "autostart": s.autostart,
        "windows": cfg!(windows),
        "error": error,
    });
    // In a <script>: nothing in it may end the element.
    let state = state.to_string().replace("</", "<\\/");
    include_str!("setup.html").replace("{{STATE}}", &state)
}

/// What a page sent the app.
#[derive(Debug, PartialEq)]
pub enum FromPage {
    /// The setup page's Connect.
    Save {
        server: String,
        pass_key: bool,
        autostart: bool,
    },
    /// Set the talk key: the next key pressed.
    Learn,
    ClearKey,
    /// Back to the Voice page.
    Cancel,
    /// A call is ringing.
    Ring,
}

impl FromPage {
    /// `text` from a page; the setup page's own messages only from the setup page
    /// (`on_setup`), so that no server can change the app's settings.
    pub fn parse(text: &str, on_setup: bool) -> Option<FromPage> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let nonce_ok = {
            let n = SETUP_NONCE.lock_ok();
            !n.is_empty() && v["nonce"].as_str() == Some(n.as_str())
        };
        let m = match v["t"].as_str()? {
            "ring" => return Some(FromPage::Ring),
            _ if !on_setup || !nonce_ok => return None,
            "save" => FromPage::Save {
                server: v["server"].as_str().unwrap_or_default().to_owned(),
                pass_key: v["pass_key"].as_bool().unwrap_or(false),
                autostart: v["autostart"].as_bool().unwrap_or(false),
            },
            "learn" => FromPage::Learn,
            "clear_key" => FromPage::ClearKey,
            "cancel" => FromPage::Cancel,
            _ => return None,
        };
        Some(m)
    }
}

/// What the certificate question says, for the server `hp` and its certificate's
/// fingerprint `fp`; `changed`: a different one was trusted there before.
pub fn certificate_question(hp: &str, fp: &str, changed: bool) -> String {
    if changed {
        format!(
            "The Voice server {hp} shows a different certificate from the one trusted before.\n\n\
             That happens when its IDN Site Manager was installed again; it could also be \
             someone else on the network pretending to be it.\n\n\
             Its certificate's SHA-256 is now:\n{fp}\n\n\
             Compare it with the one the Site Manager shows (its log: \"certificate SHA-256 …\"). \
             Trust the new certificate and connect?"
        )
    } else {
        format!(
            "The Voice server {hp} has a certificate of its own (the IDN Site Manager makes \
             one itself), so this computer can't check it for you.\n\n\
             Its certificate's SHA-256 is:\n{fp}\n\n\
             Compare it with the one the Site Manager shows (its log: \"certificate SHA-256 …\"). \
             Trust it and connect?"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setup pages' word is one for the program: tests that make pages take turns.
    static PAGES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn setup_messages_only_from_the_setup_page() {
        let _turn = PAGES.lock_ok();
        // The page made now: its word in its state.
        let page = setup_page(&settings::Settings::default(), "");
        let nonce = SETUP_NONCE.lock_ok().clone();
        assert!(page.contains(&nonce));
        let save = format!(
            r#"{{"t":"save","server":"sm","pass_key":true,"autostart":false,"nonce":"{nonce}"}}"#
        );
        let save = save.as_str();
        // Without its word (another page while the setup page shows): refused.
        assert_eq!(
            FromPage::parse(
                r#"{"t":"save","server":"evil","pass_key":true,"autostart":true}"#,
                true
            ),
            None
        );
        assert_eq!(
            FromPage::parse(save, true),
            Some(FromPage::Save {
                server: "sm".into(),
                pass_key: true,
                autostart: false
            })
        );
        // A Voice server's page can ring, nothing else.
        assert_eq!(FromPage::parse(save, false), None);
        assert_eq!(FromPage::parse(r#"{"t":"learn"}"#, false), None);
        assert_eq!(
            FromPage::parse(r#"{"t":"ring","from":"x"}"#, false),
            Some(FromPage::Ring)
        );
        assert_eq!(FromPage::parse("not json", true), None);
    }

    #[test]
    fn the_setup_page_carries_its_state_safely() {
        let _turn = PAGES.lock_ok();
        let s = settings::Settings {
            server: "https://sm:29203/voice/</script><script>alert(1)</script>".into(),
            ..Default::default()
        };
        let page = setup_page(&s, "");
        assert!(!page.contains("{{STATE}}"));
        assert!(!page.contains("</script><script>alert"));
        assert_eq!(page.matches("</script>").count(), 1);
    }
}
