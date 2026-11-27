//! Labels for the multiview: the 5×7 bitmap font (`idn_common::font5x7`) drawn scaled
//! onto a frame's luma.

use crate::frame::Frame;

pub use idn_common::font5x7::width;

/// Draws `text` in luma `y` (white: 235) with its top left at (`x`, `top`), each font pixel
/// `scale` × `scale`, clipped to `max_w` pixels wide.
pub fn draw(f: &mut Frame, text: &str, x: usize, top: usize, scale: usize, y: u8, max_w: usize) {
    let (w, h) = (f.width, f.height);
    idn_common::font5x7::draw(text, x, top, scale, max_w, |px, py| {
        if px < w && py < h {
            f.y[py * w + px] = y;
        }
    });
}
