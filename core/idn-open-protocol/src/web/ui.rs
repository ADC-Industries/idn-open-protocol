//! The IDN web panels' shared files, served by every panel at `/ui/…`: one stylesheet
//! (`base.css`: an "admin" page follows the system's light or dark scheme, a "console"
//! page is always dark and denser), the helpers every panel script uses (`ui.js`), the
//! login page, the audio routing matrix (`matrix.js`, `matrix.css`) and a machine's video
//! settings (`video.js`). Plain files built into the binaries, no build step; each panel
//! keeps only its own screens.
//!
//! A page loads `/ui/base.css` before its own `app.css`, and `/ui/ui.js` before its own
//! `app.js`; its `<body>` names its kind and the header its changes carry:
//! `<body class="admin" data-header="X-IDN-Node" data-who="machine">`.

use super::Response;

const BASE_CSS: &str = include_str!("ui/base.css");
const UI_JS: &str = include_str!("ui/ui.js");
const LOGIN_JS: &str = include_str!("ui/login.js");
const MATRIX_JS: &str = include_str!("ui/matrix.js");
const MATRIX_CSS: &str = include_str!("ui/matrix.css");
const VIDEO_JS: &str = include_str!("ui/video.js");
const SETTINGS_JS: &str = include_str!("ui/settings.js");
const SHELL_JS: &str = include_str!("ui/shell.js");
const SHELL_CSS: &str = include_str!("ui/shell.css");

const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// The shared file at `path` (`/ui/base.css`, …), for any request (they hold nothing
/// private); `None` for any other path.
pub fn asset(path: &str) -> Option<Response> {
    asset_file(path).map(|(ctype, body)| Response::ok(ctype, body))
}

/// The shared file at `path` as its content type and text, for a program that serves it
/// with its own HTTP server (the Industrial Site Manager's).
pub fn asset_file(path: &str) -> Option<(&'static str, &'static str)> {
    Some(match path {
        "/ui/base.css" => (CSS, BASE_CSS),
        "/ui/ui.js" => (JS, UI_JS),
        "/ui/login.js" => (JS, LOGIN_JS),
        "/ui/matrix.js" => (JS, MATRIX_JS),
        "/ui/matrix.css" => (CSS, MATRIX_CSS),
        "/ui/video.js" => (JS, VIDEO_JS),
        "/ui/settings.js" => (JS, SETTINGS_JS),
        "/ui/shell.js" => (JS, SHELL_JS),
        "/ui/shell.css" => (CSS, SHELL_CSS),
        _ => return None,
    })
}

/// A panel's login page: the password goes to `POST /api/v1/login` (as
/// [`PasswordGate::handle`](super::PasswordGate::handle) answers it).
pub struct Login<'a> {
    /// The program's name: the page's title and heading.
    pub title: &'a str,
    /// A line under the heading, if any.
    pub hint: Option<&'a str>,
    /// The password field's label ("Password", "Site password").
    pub label: &'a str,
    /// The header the login request carries (`X-IDN-Node`).
    pub header: &'a str,
    /// What to call the program when it doesn't answer ("machine", "switcher").
    pub who: &'a str,
    /// A console page (always dark) rather than a settings page.
    pub console: bool,
    /// A user name field too (the Site Manager's users, idn-node.md §N.31); left empty,
    /// the password alone.
    pub user: bool,
}

/// The login page's HTML.
pub fn login_page(l: &Login) -> String {
    let hint = l
        .hint
        .map(|h| format!("\n    <p class=\"hint\">{}</p>", idn_common::html::escape(h)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="/ui/base.css">
</head>
<body class="{kind}" data-header="{header}" data-who="{who}">
<main>
  <section class="card login">
    <h2>{title}</h2>{hint}
    <form id="login">{user}
      <label>{label} <input name="password" type="password" autocomplete="current-password" required autofocus></label>
      <div class="actions"><button type="submit" class="primary">Log in</button></div>
      <p class="error" id="error" hidden></p>
    </form>
  </section>
</main>
<script src="/ui/ui.js"></script>
<script src="/ui/login.js"></script>
</body>
</html>
"#,
        title = idn_common::html::escape(l.title),
        kind = if l.console { "console" } else { "admin" },
        header = idn_common::html::escape(l.header),
        who = idn_common::html::escape(l.who),
        label = idn_common::html::escape(l.label),
        user = if l.user {
            "\n      <label>User <input name=\"user\" autocomplete=\"username\" placeholder=\"blank for the site password\"></label>"
        } else {
            ""
        },
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shared_file_is_served_and_others_are_not() {
        for p in [
            "/ui/base.css",
            "/ui/ui.js",
            "/ui/login.js",
            "/ui/matrix.js",
            "/ui/matrix.css",
            "/ui/video.js",
        ] {
            let r = asset(p).unwrap_or_else(|| panic!("{p} not served"));
            assert!(!r.body.is_empty(), "{p} empty");
        }
        assert!(asset("/ui/").is_none());
        assert!(asset("/ui/../web.rs").is_none());
        assert!(asset("/app.js").is_none());
    }

    #[test]
    fn the_login_page_names_the_program_and_escapes_what_it_is_given() {
        let page = login_page(&Login {
            title: "IDN <Switcher>",
            hint: Some("St Mark's \"AV\""),
            label: "Password",
            header: "X-IDN-Switcher",
            who: "switcher",
            console: true,
            user: false,
        });
        assert!(page.contains("<title>IDN &lt;Switcher&gt;</title>"));
        assert!(page.contains("St Mark&#39;s &quot;AV&quot;"));
        assert!(page.contains(
            r#"<body class="console" data-header="X-IDN-Switcher" data-who="switcher">"#
        ));
        assert!(page.contains(r#"<script src="/ui/login.js">"#));
    }
}
