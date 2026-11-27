//! HTML escaping for pages made by hand (the panels' login page, the phones' listening
//! page, the user guide).

/// `s` safe as HTML text or a quoted attribute: `& < > " '` escaped.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn markup_and_quotes_are_escaped() {
        assert_eq!(
            super::escape(r#"<b>"Grace" & 'co'</b>"#),
            "&lt;b&gt;&quot;Grace&quot; &amp; &#39;co&#39;&lt;/b&gt;"
        );
    }
}
