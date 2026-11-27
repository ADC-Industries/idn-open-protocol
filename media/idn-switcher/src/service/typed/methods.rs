//! A command's name, body and parsing, and the service's typed shortcuts that run one.

use super::*;

impl Command {
    /// Every command name, as [`Service::command`] takes them.
    pub const NAMES: &'static [&'static str] = &[
        "mqtt_set",
        "starter",
        "undo",
        "redo",
        "add_input",
        "list",
        "list_items",
        "media",
        "filters",
        "title",
        "title_fields",
        "camera",
        "ptz",
        "virtual_view",
        "title_row",
        "title_field_add",
        "title_template",
        "timer",
        "slideshow",
        "slide",
        "scene_item_sound",
        "scene_items",
        "scene_layer_visible",
        "global_layer",
        "global_layer_remove",
        "keep_active",
        "scene_snapshot",
        "format",
        "destination_save",
        "destination_remove",
        "go_live",
        "multiview_layout",
        "multiview",
        "remove_input",
        "rename_input",
        "audio_offset",
        "preview",
        "program",
        "cut",
        "auto",
        "tbar",
        "transition",
        "stinger",
        "ftb",
        "audio",
        "monitor",
        "monitor_solo",
        "screen",
        "caption",
        "captions",
        "setup_save",
        "setup_load",
        "setup_delete",
        "cue_go",
        "cue_back",
        "cue_standby",
        "cue_fire",
        "show_set",
        "cue_add",
        "cue_update",
        "cue_delete",
        "cue_move",
        "show_new",
        "show_load",
        "show_save",
        "show_delete",
        "pco_login",
        "pco_logout",
        "pco_plans",
        "pco_import",
        "stream_add",
        "iso_add",
        "add_mix",
        "remove_mix",
        "mix_program",
        "mix_preview",
        "mix_cut",
        "mix_auto",
        "mix_transition",
        "stream_pause",
        "stream_remove",
    ];

    /// The command's whole JSON: `{"cmd": name, "body": {...}}`.
    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The command's name (`POST /api/v1/<name>`).
    pub fn name(&self) -> &'static str {
        let v = self.to_json();
        let cmd = v["cmd"].as_str().unwrap_or("");
        Self::NAMES
            .iter()
            .copied()
            .find(|n| *n == cmd)
            .unwrap_or("")
    }

    /// The command's JSON body, as [`Service::command`] takes it.
    pub fn body(&self) -> Value {
        match self.to_json()["body"].take() {
            Value::Null => json!({}),
            b => b,
        }
    }

    /// A command from its name and JSON body; why not if the body doesn't fit.
    pub fn parse(name: &str, body: &Value) -> Result<Command, String> {
        let body = if body.is_null() {
            json!({})
        } else {
            body.clone()
        };
        serde_json::from_value(json!({"cmd": name, "body": body}))
            .map_err(|e| format!("{name}: {e}"))
    }
}

impl Service {
    /// Runs a typed command: [`Service::command`] with its name and body.
    pub fn run(&self, c: Command) -> Result<Value, String> {
        self.command(c.name(), &c.body())
    }

    /// Swaps preview and program at once.
    pub fn cut(&self) -> Result<Value, String> {
        self.run(Command::Cut {})
    }

    /// Swaps preview and program with the transition.
    pub fn auto(&self) -> Result<Value, String> {
        self.run(Command::Auto {})
    }

    /// Fades the program to black, or back.
    pub fn ftb(&self) -> Result<Value, String> {
        self.run(Command::Ftb {})
    }

    /// Puts a scene on program at once.
    pub fn program(&self, id: impl Into<InputRef>) -> Result<Value, String> {
        self.run(Command::Program { id: id.into() })
    }

    /// Puts a scene on preview.
    pub fn preview(&self, id: impl Into<InputRef>) -> Result<Value, String> {
        self.run(Command::Preview { id: id.into() })
    }

    /// Sets the program's transition: `kind` (cut, mix, fade, wipe-…, stinger-1…8) and
    /// its length in seconds.
    pub fn transition(&self, kind: &str, duration: f64) -> Result<Value, String> {
        self.run(Command::Transition {
            kind: Some(kind.to_owned()),
            duration: Some(duration),
            soft: None,
            pattern: None,
            invert: None,
        })
    }

    /// Shows or hides layer `layer` (its id) of scene `scene`, with its fade.
    pub fn scene_layer_visible(
        &self,
        scene: impl Into<InputRef>,
        layer: u32,
        on: bool,
    ) -> Result<Value, String> {
        self.run(Command::SceneLayerVisible {
            scene: scene.into(),
            layer,
            on: Some(on),
            fade: None,
        })
    }

    /// Fires the next cue of the show; answers the cue's errors.
    pub fn cue_go(&self) -> Result<Value, String> {
        self.run(Command::CueGo { if_next: None })
    }

    /// Fires the cue before the current one again.
    pub fn cue_back(&self) -> Result<Value, String> {
        self.run(Command::CueBack {})
    }
}
