//! Remote control over IDN (roadmap R.25 stage 1, idn-extensions.md §E.25): a receiver's
//! keyboard and mouse sent to the machine whose screen a source shows, on the connection
//! that carries its video. The source says it takes control with its capabilities
//! (`idn_input="true"`, [`CAPABILITY`]); a receiver sends events as metadata
//! ([`Input::send_control`](crate::input::Input::send_control)); the source takes those
//! from devices of its site only ([`Output::recv_control`](crate::api::Output::recv_control))
//! and injects them.
//!
//! The message: `<idn_input>` with the events in order —
//! `<m x y/>` the pointer at a place in the picture (fractions 0–1 of its width and
//! height), `<b n d/>` a button (0 left, 1 right, 2 middle, 3 back, 4 forward) down
//! (`d="1"`) or up, `<w x y/>` the wheel (in 120ths of a notch, `y` positive away from
//! the user, `x` to the right), `<k c d/>` a key by its physical place (the browser's
//! `KeyboardEvent.code`: `KeyA`, `Enter`, `ShiftLeft`; layouts don't matter) down or up,
//! `<sas/>` the secure attention sequence (Ctrl+Alt+Del: the viewer can't pass it on as
//! keys, its own system takes them), `<c>TEXT</c>` text for the clipboard (the viewer's,
//! sent before a paste; at most [`MAX_CLIPBOARD`] bytes).

use crate::xml::Element;

/// The capability attribute (in `ndi_capabilities`) of a source that takes control.
pub const CAPABILITY: (&str, &str) = ("idn_input", "true");

/// The most events one message carries.
pub const MAX_EVENTS: usize = 256;

/// The most clipboard text one message carries, bytes.
pub const MAX_CLIPBOARD: usize = 64 * 1024;

/// One keyboard or mouse event.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// The pointer at a place in the picture.
    Move {
        /// Fraction 0–1 of the picture's width.
        x: f32,
        /// Fraction 0–1 of its height.
        y: f32,
    },
    /// A button pressed or released.
    Button {
        /// 0 left, 1 right, 2 middle, 3 back, 4 forward.
        button: u8,
        /// Pressed (else released).
        down: bool,
    },
    /// The wheel turned.
    Wheel {
        /// 120ths of a notch to the right.
        x: i32,
        /// 120ths of a notch away from the user.
        y: i32,
    },
    /// A key pressed or released.
    Key {
        /// Its physical place, as the browser's `KeyboardEvent.code` (`KeyA`, `Enter`).
        code: String,
        /// Pressed (else released).
        down: bool,
    },
    /// Ctrl+Alt+Del (the secure attention sequence).
    SecureAttention,
    /// Text for the controlled machine's clipboard (the viewer's, before a paste).
    Clipboard(String),
    /// The pointer moved by this much (the mouse's own counts, as a game reads them: the
    /// viewer's pointer locked to its window; R.25 stage 2).
    Rel {
        /// Counts to the right.
        dx: i32,
        /// Counts down.
        dy: i32,
    },
    /// A gamepad's state (R.25 stage 2): which pad (0–3), its buttons ([`PAD_BUTTONS`]'s
    /// order, bit 0 first) and axes (left stick x, y, right stick x, y: −32768 to 32767,
    /// y down; left and right triggers: 0 to 32767).
    Pad {
        /// Which pad, 0–3.
        id: u8,
        /// A bit a button.
        buttons: u32,
        /// Sticks and triggers.
        axes: [i16; 6],
    },
    /// A latency probe (R.25 stage 2): the source answers with `<idn_probe id/>` right
    /// after its next picture ([`probe_answer`]), and the viewer times the round trip.
    Probe(u32),
}

/// A gamepad's buttons in [`Control::Pad`]'s bit order (the standard layout: south is A on
/// an Xbox pad, cross on a PlayStation one).
pub const PAD_BUTTONS: [&str; 15] = [
    "south", "east", "west", "north", "lb", "rb", "select", "start", "mode", "lstick", "rstick",
    "up", "down", "left", "right",
];

/// The source's answer to [`Control::Probe`] `id`, sent as metadata after its next picture.
pub fn probe_answer(id: u32) -> String {
    format!(r#"<idn_probe id="{id}"/>"#)
}

/// The probe a metadata message answers, if it is one.
pub fn probe_of(xml: &str) -> Option<u32> {
    let e = crate::xml::parse(xml)?;
    (e.name == "idn_probe")
        .then(|| e.attr("id")?.parse().ok())
        .flatten()
}

/// The most a relative move carries, each way.
const MAX_REL: i32 = 10_000;
/// The buttons a pad has ([`PAD_BUTTONS`]).
const PAD_MASK: u32 = (1 << PAD_BUTTONS.len()) - 1;

/// A key code as sent: 1–32 ASCII letters and digits.
fn valid_code(c: &str) -> bool {
    (1..=32).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The `<idn_input>` message for `events` (at most [`MAX_EVENTS`]; bad key codes left out).
pub fn to_xml(events: &[Control]) -> String {
    let mut e = Element::new("idn_input");
    for ev in events.iter().take(MAX_EVENTS) {
        let child = match ev {
            Control::Move { x, y } => Element::new("m")
                .with_attr("x", &format!("{:.5}", x.clamp(0.0, 1.0)))
                .with_attr("y", &format!("{:.5}", y.clamp(0.0, 1.0))),
            Control::Button { button, down } => Element::new("b")
                .with_attr("n", &button.to_string())
                .with_attr("d", if *down { "1" } else { "0" }),
            Control::Wheel { x, y } => Element::new("w")
                .with_attr("x", &x.to_string())
                .with_attr("y", &y.to_string()),
            Control::Key { code, down } if valid_code(code) => Element::new("k")
                .with_attr("c", code)
                .with_attr("d", if *down { "1" } else { "0" }),
            Control::Key { .. } => continue,
            Control::SecureAttention => Element::new("sas"),
            Control::Clipboard(t) if t.len() <= MAX_CLIPBOARD => Element::new("c").with_text(t),
            Control::Clipboard(_) => continue,
            Control::Rel { dx, dy } => Element::new("r")
                .with_attr("x", &(*dx).clamp(-MAX_REL, MAX_REL).to_string())
                .with_attr("y", &(*dy).clamp(-MAX_REL, MAX_REL).to_string()),
            Control::Pad { id, buttons, axes } => Element::new("p")
                .with_attr("i", &id.min(&3).to_string())
                .with_attr("b", &(buttons & PAD_MASK).to_string())
                .with_attr(
                    "a",
                    &axes
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            Control::Probe(id) => Element::new("probe").with_attr("id", &id.to_string()),
        };
        e = e.with_child(child);
    }
    e.to_xml()
}

/// The events of an `<idn_input>` message; `None` for any other message. Events that
/// don't read right are left out.
pub fn from_xml(xml: &str) -> Option<Vec<Control>> {
    let e = crate::xml::parse(xml)?;
    if e.name != "idn_input" {
        return None;
    }
    let f = |c: &Element, k: &str| c.attr(k).and_then(|v| v.parse::<f32>().ok());
    let i = |c: &Element, k: &str| c.attr(k).and_then(|v| v.parse::<i32>().ok());
    let down = |c: &Element| c.attr("d") == Some("1");
    Some(
        e.children
            .iter()
            .take(MAX_EVENTS)
            .filter_map(|c| match c.name.as_str() {
                "m" => {
                    let (x, y) = (f(c, "x")?, f(c, "y")?);
                    (x.is_finite() && y.is_finite()).then(|| Control::Move {
                        x: x.clamp(0.0, 1.0),
                        y: y.clamp(0.0, 1.0),
                    })
                }
                "b" => Some(Control::Button {
                    button: c.attr("n")?.parse().ok().filter(|n| *n <= 4)?,
                    down: down(c),
                }),
                "w" => Some(Control::Wheel {
                    x: i(c, "x").unwrap_or(0).clamp(-12_000, 12_000),
                    y: i(c, "y").unwrap_or(0).clamp(-12_000, 12_000),
                }),
                "k" => c
                    .attr("c")
                    .filter(|v| valid_code(v))
                    .map(|code| Control::Key {
                        code: code.to_owned(),
                        down: down(c),
                    }),
                "sas" => Some(Control::SecureAttention),
                "r" => Some(Control::Rel {
                    dx: i(c, "x").unwrap_or(0).clamp(-MAX_REL, MAX_REL),
                    dy: i(c, "y").unwrap_or(0).clamp(-MAX_REL, MAX_REL),
                }),
                "p" => {
                    let id: u8 = c.attr("i")?.parse().ok().filter(|n| *n <= 3)?;
                    let buttons = c.attr("b")?.parse::<u32>().ok()? & PAD_MASK;
                    let v: Vec<i16> = c
                        .attr("a")?
                        .split(',')
                        .map(|a| a.trim().parse::<i16>().ok())
                        .collect::<Option<_>>()?;
                    let axes: [i16; 6] = v.try_into().ok()?;
                    Some(Control::Pad { id, buttons, axes })
                }
                "probe" => Some(Control::Probe(c.attr("id")?.parse().ok()?)),
                "c" if c.text.len() <= MAX_CLIPBOARD => Some(Control::Clipboard(c.text.clone())),
                _ => None,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_go_and_come_back_as_they_were() {
        let ev = vec![
            Control::Move { x: 0.25, y: 0.75 },
            Control::Button {
                button: 0,
                down: true,
            },
            Control::Wheel { x: 0, y: -120 },
            Control::Key {
                code: "KeyA".into(),
                down: true,
            },
            Control::Key {
                code: "ShiftLeft".into(),
                down: false,
            },
            Control::Button {
                button: 0,
                down: false,
            },
            Control::SecureAttention,
            Control::Clipboard("a <b> & \"c\"\nline two".into()),
            Control::Rel { dx: -5, dy: 12 },
            Control::Pad {
                id: 1,
                buttons: 0b101,
                axes: [-32768, 32767, 0, -1, 0, 32767],
            },
            Control::Probe(77),
        ];
        assert_eq!(from_xml(&to_xml(&ev)).unwrap(), ev);
        // Clipboard text past the limit isn't sent.
        let big = Control::Clipboard("x".repeat(MAX_CLIPBOARD + 1));
        assert_eq!(from_xml(&to_xml(&[big])).unwrap(), vec![]);
    }

    #[test]
    fn what_doesnt_read_right_is_left_out() {
        // Off the picture: held at its edge; a sixth button, a key code with a quote or
        // too long, a non-number: left out; another message: not ours.
        let got = from_xml(
            r#"<idn_input><m x="1.5" y="-2"/><b n="7" d="1"/><k c="a&quot;b" d="1"/><k c="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" d="1"/><m x="NaN" y="0"/><w y="240"/></idn_input>"#,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                Control::Move { x: 1.0, y: 0.0 },
                Control::Wheel { x: 0, y: 240 }
            ]
        );
        assert!(from_xml("<ntk_ptz_zoom zoom=\"1\"/>").is_none());
        // A fifth pad, five axes, a huge move: left out or held at the limit.
        let got = from_xml(r#"<idn_input><p i="4" b="1" a="0,0,0,0,0,0"/><p i="0" b="1" a="0,0,0,0,0"/><r x="99999" y="0"/></idn_input>"#).unwrap();
        assert_eq!(got, vec![Control::Rel { dx: MAX_REL, dy: 0 }]);
        assert_eq!(probe_of(&probe_answer(9)), Some(9));
        assert_eq!(probe_of("<x_idn_tally/>"), None);
        // A code that isn't sent at all.
        let x = to_xml(&[Control::Key {
            code: "<x>".into(),
            down: true,
        }]);
        assert_eq!(from_xml(&x).unwrap(), vec![]);
    }
}
