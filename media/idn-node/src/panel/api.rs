//! The panel's API: each page and request (`GET`/`POST /api/v1/…`), for a session
//! that is logged in (`ok`) or not, on this machine or the network (`lan`).

use super::*;

impl State {
    /// The pages and the API's GETs, for a logged-in (or token) session (`ok`) or not; the
    /// changes are commands (panel/commands.rs) through the shared API front.
    pub(super) fn route(&self, r: Request, ok: bool, lan: bool) -> Response {
        if let Some(a) = web::ui::asset(&r.path).filter(|_| r.method == "GET") {
            return a;
        }
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/app.js") => Response::ok("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/pages.js") => Response::ok("text/javascript; charset=utf-8", PAGES_JS),
            ("GET", "/wizard.js") => Response::ok("text/javascript; charset=utf-8", WIZARD_JS),
            ("GET", "/app.css") => Response::ok("text/css; charset=utf-8", APP_CSS),
            ("GET", "/") if ok => Response::ok("text/html; charset=utf-8", INDEX),
            ("GET", "/") => Response::new("403 Forbidden", "text/html; charset=utf-8", LOCKED),
            (_, p) if p.starts_with("/api/") && !ok => {
                Response::text("403 Forbidden", "not logged in")
            }
            // The panel's audit (§N.31.6): newest first.
            ("GET", "/api/v1/audit") => web::audit::answer(&self.audit, &r),
            ("GET", "/api/v1/status") => {
                let mut v = self.status();
                v["lan"] = Value::Bool(lan);
                Response::json(v.to_string())
            }
            // The transmitters' levels for the matrix's meters, while it watches
            // (idn-routing.md §A.15): `{"tx": {name: [dB, …]}}`.
            ("GET", "/api/v1/levels") => Response::json(
                serde_json::json!({
                    "tx": idn_open_protocol::node::meters::levels_json(&self.core.levels_element())
                })
                .to_string(),
            ),
            ("GET", "/api/v1/logs") => match r.param("app") {
                None => Response::json(
                    json!({"programs": self.core.program_logs().iter().map(|(a, p)| json!({"app": a, "file": p.display().to_string()})).collect::<Vec<_>>()})
                        .to_string(),
                ),
                Some(app) => {
                    let n = r.param("lines").and_then(|v| v.parse().ok()).unwrap_or(200);
                    match self.core.program_log_tail(app, n) {
                        Some(l) => Response::json(json!({"app": app, "lines": l}).to_string()),
                        None => Response::json_err("404 Not Found", "no log for that program"),
                    }
                }
            },
            // The site's newest release for this machine, asked for now (§N.42).
            _ => Response::text("404 Not Found", "not found"),
        }
    }
}
