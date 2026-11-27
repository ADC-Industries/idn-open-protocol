//! The web client's files (§V.11) and the QR code of a channel's link.

pub const INDEX_HTML: &str = include_str!("web/index.html");
pub const APP_JS: &str = include_str!("web/app.js");
pub const APP_CSS: &str = include_str!("web/app.css");

/// A QR code of `text` as SVG.
pub fn qr_svg(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(q) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return String::new();
    };
    let (n, border) = (q.size(), 4);
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if q.get_module(x, y) {
                path += &format!("M{},{}h1v1h-1z", x + border, y + border);
            }
        }
    }
    let side = n + 2 * border;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {side} {side}\" shape-rendering=\"crispEdges\" role=\"img\" aria-label=\"QR code\"><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#000\"/></svg>"
    )
}
