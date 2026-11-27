//! The program format changed from the API (a "runtime format", not while live):
//! kept in the service's folder for the next start, and, run by the IDN Node, the switcher
//! started again at once with it (the engine's format is set at its start). Its scenes,
//! setups, shows and settings are saved as they change, so they come back.

use super::*;

/// The format kept for the next start.
pub(super) const FORMAT_FILE: &str = "format.json";

/// The format kept in `dir` for the next start: (width, height, frame rate, as asked).
pub fn saved_format(dir: &std::path::Path) -> Option<(usize, usize, (u32, u32), String)> {
    let v: Value = super::saved::load_state(&dir.join(FORMAT_FILE))?;
    let f = v["format"].as_str()?.to_owned();
    let (w, h, fps) = parse_format(&f)?;
    Some((w, h, fps, f))
}

impl Service {
    /// `format {format}` (`1080p30`, `1080p60`, `2160p30`, `WxH@FPS`): refused while
    /// anything streams or records; kept, and under the IDN Node the switcher starts again
    /// with it at once (`{"restart": true}`); run by hand, from its next start
    /// (`{"restart_needed": true}`, the state's `format_next`).
    pub(super) fn set_format(&self, v: &Value) -> Result<Value, String> {
        let f = v["format"]
            .as_str()
            .map(str::trim)
            .ok_or("format: 1080p30, 1080p60, 2160p30, 2160p60 or WxH@FPS")?;
        let (w, h, fps) =
            parse_format(f).ok_or("format: 1080p30, 1080p60, 2160p30, 2160p60 or WxH@FPS")?;
        if self
            .streams
            .state()
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            return Err("not while it streams or records: stop them first".into());
        }
        let b = serde_json::to_vec(&json!({ "format": f })).map_err(|e| e.to_string())?;
        idn_common::fsutil::write_atomic(&state_dir().join(FORMAT_FILE), &b)
            .map_err(|e| format!("saving the format: {e}"))?;
        let now = self.sw.state()["format"].clone();
        let same = now["width"] == w && now["height"] == h && now["fps"] == json!([fps.0, fps.1]);
        if same {
            return Ok(json!({ "restart": false }));
        }
        self.show_version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if idn_common::switches::on("IDN_SUPERVISED") {
            idn_open_protocol::log::log(&format!(
                "switcher: the format is now {f}: starting again"
            ));
            // (After the answer has gone.)
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                std::process::exit(idn_common::supervisor::AGAIN_NOW);
            });
            return Ok(json!({ "restart": true }));
        }
        Ok(json!({ "restart_needed": true }))
    }

    /// The format kept for the next start, when it isn't the one running.
    pub(super) fn format_next(&self) -> Option<String> {
        let (w, h, fps, f) = saved_format(&state_dir())?;
        let now = self.sw.state()["format"].clone();
        let same = now["width"] == w && now["height"] == h && now["fps"] == json!([fps.0, fps.1]);
        (!same).then_some(f)
    }
}
