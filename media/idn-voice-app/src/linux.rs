//! Linux: the Voice page in WebKitGTK. No talk key from other programs yet (Wayland lets
//! no program see keys meant for others); the page's own space bar works. No tray: the
//! window closes the program; the start at log-in opens it minimised.

use crate::settings::{self, Settings};
use crate::{BRIDGE, FromPage};
use gtk::prelude::*;
use gtk::{gdk_pixbuf, gio, glib};
use javascriptcore::ValueExt;
use std::cell::RefCell;
use std::rc::Rc;
use webkit2gtk::{
    CookieManagerExt, CookiePersistentStorage, NavigationPolicyDecision,
    NavigationPolicyDecisionExt, PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType,
    SettingsExt, URIRequestExt, UserContentInjectedFrames, UserContentManager,
    UserContentManagerExt, UserMediaPermissionRequest, UserMediaPermissionRequestExt, UserScript,
    UserScriptInjectionTime, WebContext, WebContextExt, WebView, WebViewExt, WebsiteDataManager,
};

const ICON: &[u8] = include_bytes!("../../idn-node/assets/idn-256.png");

struct State {
    settings: Settings,
    on_setup: bool,
    asking: bool,
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let hidden = args.iter().any(|a| a == "--hidden");
    let want_setup = args.iter().any(|a| a == "--setup");
    if gtk::init().is_err() {
        eprintln!("idn-voice: no display");
        std::process::exit(3);
    }
    // One window per user: a second start brings it forward.
    let app = gtk::Application::new(
        Some("com.adcindustries.idn.Voice"),
        gio::ApplicationFlags::empty(),
    );
    app.connect_activate(move |app| {
        if let Some(w) = app
            .active_window()
            .or_else(|| app.windows().into_iter().next())
        {
            w.present();
            return;
        }
        build(app, hidden, want_setup);
    });
    std::process::exit(app.run_with_args::<&str>(&[]).into());
}

fn build(app: &gtk::Application, hidden: bool, want_setup: bool) {
    let win = gtk::ApplicationWindow::new(app);
    win.set_title("IDN Voice");
    win.set_default_size(520, 820);
    win.set_position(gtk::WindowPosition::Center);
    if let Ok(icon) = gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(ICON)) {
        win.set_icon(Some(&icon));
    }
    // The Voice session's cookie kept in the user's folder.
    let base = glib::user_data_dir().join("idn/voice-app");
    let data = WebsiteDataManager::builder()
        .base_data_directory(base.join("data").to_string_lossy().into_owned())
        .base_cache_directory(
            glib::user_cache_dir()
                .join("idn/voice-app")
                .to_string_lossy()
                .into_owned(),
        )
        .build();
    let context = WebContext::with_website_data_manager(&data);
    if let Some(cookies) = context.cookie_manager() {
        cookies.set_persistent_storage(
            &base.join("cookies.sqlite").to_string_lossy(),
            CookiePersistentStorage::Sqlite,
        );
    }
    let content = UserContentManager::new();
    content.add_script(&UserScript::new(
        BRIDGE,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content.register_script_message_handler("idnVoice");
    let view = WebView::builder()
        .web_context(&context)
        .user_content_manager(&content)
        .build();
    if let Some(s) = WebViewExt::settings(&view) {
        s.set_enable_developer_extras(false);
        s.set_enable_media_stream(true);
        s.set_enable_webrtc(true);
        s.set_enable_webaudio(true);
        s.set_media_playback_requires_user_gesture(false);
    }
    let state = Rc::new(RefCell::new(State {
        settings: Settings::load(),
        on_setup: false,
        asking: false,
    }));
    // Links that open a new window: the browser.
    view.connect_decide_policy(|_, decision, kind| {
        if kind != PolicyDecisionType::NewWindowAction {
            return false;
        }
        let uri = decision
            .downcast_ref::<NavigationPolicyDecision>()
            .and_then(|d| d.navigation_action())
            .and_then(|a| a.request())
            .and_then(|r| r.uri());
        if let Some(uri) = uri.filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
            let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
        }
        decision.ignore();
        true
    });
    // The microphone: allowed for the Voice server without asking each time.
    {
        let state = state.clone();
        view.connect_permission_request(move |v, req| {
            let Some(media) = req.downcast_ref::<UserMediaPermissionRequest>() else {
                return false;
            };
            let server = state.borrow().settings.server.clone();
            let here = v.uri().map(|u| u.to_string()).unwrap_or_default();
            if media.is_for_audio_device()
                && !media.is_for_video_device()
                && !server.is_empty()
                && settings::same_origin(&here, &server)
            {
                req.allow();
            } else {
                req.deny();
            }
            true
        });
    }
    // The Site Manager's own certificate: trusted once the user compared it.
    {
        let (state, win, context) = (state.clone(), win.clone(), context.clone());
        view.connect_load_failed_with_tls_errors(move |v, uri, cert, _| {
            let der = cert.certificate_pem().and_then(|p| settings::pem_der(&p));
            let Some(fp) = der.map(|d| settings::fingerprint(&d)) else {
                return false;
            };
            let hp = settings::host_port(uri).unwrap_or_default();
            let host = hp
                .rsplit_once(':')
                .map(|(h, _)| h.trim_matches(['[', ']']))
                .unwrap_or("")
                .to_owned();
            let (server, trusted) = {
                let s = state.borrow();
                (
                    s.settings.server.clone(),
                    s.settings.trusted.get(&hp).cloned(),
                )
            };
            if trusted.as_deref() == Some(fp.as_str()) {
                context.allow_tls_certificate_for_host(cert, &host);
                v.load_uri(uri);
                return true;
            }
            if !settings::same_origin(uri, &server) || state.borrow().asking {
                return false;
            }
            state.borrow_mut().asking = true;
            let q = crate::certificate_question(&hp, &fp, trusted.is_some());
            let d = gtk::MessageDialog::new(
                Some(&win),
                gtk::DialogFlags::MODAL,
                if trusted.is_some() {
                    gtk::MessageType::Warning
                } else {
                    gtk::MessageType::Question
                },
                gtk::ButtonsType::YesNo,
                &q,
            );
            d.set_default_response(gtk::ResponseType::No);
            let yes = d.run() == gtk::ResponseType::Yes;
            // SAFETY: GTK widget destruction on its own thread.
            unsafe { d.destroy() };
            state.borrow_mut().asking = false;
            if yes {
                {
                    let mut s = state.borrow_mut();
                    s.settings.trusted.insert(hp.clone(), fp.clone());
                    let _ = s.settings.save();
                }
                context.allow_tls_certificate_for_host(cert, &host);
                v.load_uri(uri);
            } else {
                show_setup(
                    v,
                    &state,
                    "Not connected: the server's certificate wasn't trusted.",
                );
            }
            true
        });
    }
    // The Voice page didn't load: the setup page, saying so.
    {
        let state = state.clone();
        view.connect_load_failed(move |v, _, _, err| {
            if err.matches(webkit2gtk::NetworkError::Cancelled) {
                return false;
            }
            let (on_setup, asking, server) = {
                let s = state.borrow();
                (s.on_setup, s.asking, s.settings.server.clone())
            };
            if on_setup || asking {
                return false;
            }
            let v = v.clone();
            let state = state.clone();
            // Not from inside the failing load.
            glib::idle_add_local_once(move || {
                show_setup(&v, &state, &format!("Couldn't reach the Voice server at {server}. Check the address, and that this computer is on the site's network."));
            });
            true
        });
    }
    // A call rings; the setup page's choices.
    {
        let (state, win, view2) = (state.clone(), win.clone(), view.clone());
        content.connect_script_message_received(Some("idnVoice"), move |_, r| {
            let Some(text) = r.js_value().map(|v| v.to_str().to_string()) else {
                return;
            };
            let on_setup = state.borrow().on_setup;
            match FromPage::parse(&text, on_setup) {
                Some(FromPage::Ring) => {
                    win.deiconify();
                    win.present();
                    win.set_urgency_hint(true);
                }
                Some(FromPage::Save { server, autostart, .. }) => {
                    let Some(url) = settings::server_url(&server) else {
                        show_setup(&view2, &state, "That isn't an address: type the Site Manager's name or address, or the whole Voice link.");
                        return;
                    };
                    set_autostart(autostart);
                    {
                        let mut s = state.borrow_mut();
                        s.settings.server = url.clone();
                        s.settings.autostart = autostart;
                        let _ = s.settings.save();
                        s.on_setup = false;
                    }
                    view2.load_uri(&url);
                }
                Some(FromPage::Cancel) => {
                    let server = state.borrow().settings.server.clone();
                    if !server.is_empty() {
                        state.borrow_mut().on_setup = false;
                        view2.load_uri(&server);
                    }
                }
                // (The talk key is Windows only.)
                Some(FromPage::Learn | FromPage::ClearKey) | None => {}
            }
        });
    }
    win.connect_focus_in_event(|w, _| {
        w.set_urgency_hint(false);
        glib::Propagation::Proceed
    });
    let server = state.borrow().settings.server.clone();
    if server.is_empty() || want_setup {
        show_setup(&view, &state, "");
    } else {
        view.load_uri(&server);
    }
    win.add(&view);
    win.show_all();
    if hidden && !server.is_empty() {
        win.iconify();
    }
}

fn show_setup(view: &WebView, state: &Rc<RefCell<State>>, error: &str) {
    let page = {
        let mut s = state.borrow_mut();
        s.on_setup = true;
        crate::setup_page(&s.settings, error)
    };
    view.load_html(&page, Some("about:blank"));
}

/// The desktop's autostart entry (`~/.config/autostart/idn-voice.desktop`), minimised.
fn set_autostart(on: bool) {
    let Some(dir) =
        settings::path().and_then(|p| p.parent()?.parent().map(|d| d.join("autostart")))
    else {
        return;
    };
    let file = dir.join("idn-voice.desktop");
    if !on {
        let _ = std::fs::remove_file(file);
        return;
    }
    let exe = std::env::current_exe()
        .map(|e| e.display().to_string())
        .unwrap_or_else(|_| "idn-voice".into());
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        file,
        format!(
            "[Desktop Entry]\nType=Application\nName=IDN Voice\nExec=\"{exe}\" --hidden\nIcon=idn\nX-GNOME-Autostart-enabled=true\n"
        ),
    );
}
