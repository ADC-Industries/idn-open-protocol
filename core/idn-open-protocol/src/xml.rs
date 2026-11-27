//! A small XML tree parser and writer for protocol messages. It handles elements,
//! attributes, text, the five predefined entities and character references; it rejects
//! DTDs and never resolves external entities. Depth and size are bounded.

use crate::control::escape_attr;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;
/// Attributes on one element, at most (they also count as nodes): a 4 MB message of
/// tiny attributes used to cost ~100 MB (R1-55).
const MAX_ATTRS: usize = 256;

impl Element {
    pub fn new(name: &str) -> Self {
        Element {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Text of a direct child, or "" if absent.
    pub fn child_text(&self, name: &str) -> &str {
        self.child(name).map_or("", |c| c.text.as_str())
    }

    pub fn with_attr(mut self, k: &str, v: &str) -> Self {
        self.attrs.push((k.into(), v.into()));
        self
    }

    pub fn with_child(mut self, c: Element) -> Self {
        self.children.push(c);
        self
    }

    pub fn with_text(mut self, t: &str) -> Self {
        self.text = t.into();
        self
    }

    /// Serialises without whitespace; empty elements are written `<x/>`.
    pub fn to_xml(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    fn write(&self, s: &mut String) {
        s.push('<');
        s.push_str(&self.name);
        for (k, v) in &self.attrs {
            s.push_str(&format!(" {k}=\"{}\"", escape_attr(v)));
        }
        if self.children.is_empty() && self.text.is_empty() {
            s.push_str("/>");
            return;
        }
        s.push('>');
        s.push_str(&escape_attr(&self.text));
        for c in &self.children {
            c.write(s);
        }
        s.push_str(&format!("</{}>", self.name));
    }
}

/// Parses a sequence of sibling elements (an XML fragment), e.g.
/// `<query/><receiver/><advertiser/>`.
pub fn parse_fragment(s: &str) -> Option<Vec<Element>> {
    let mut p = P {
        s: s.as_bytes(),
        i: 0,
        nodes: 0,
    };
    let mut out = Vec::new();
    loop {
        p.skip_ws_and_misc()?;
        if p.i >= p.s.len() {
            return Some(out);
        }
        out.push(p.element(0)?);
    }
}

/// Parses exactly one element (surrounding whitespace allowed).
pub fn parse(s: &str) -> Option<Element> {
    let mut v = parse_fragment(s)?;
    if v.len() == 1 { v.pop() } else { None }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    nodes: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn starts(&self, t: &str) -> bool {
        self.s[self.i..].starts_with(t.as_bytes())
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    /// Skips whitespace, comments and processing instructions; rejects DTDs.
    fn skip_ws_and_misc(&mut self) -> Option<()> {
        loop {
            self.skip_ws();
            if self.starts("<!--") {
                let end = find(&self.s[self.i..], b"-->")?;
                self.i += end + 3;
            } else if self.starts("<?") {
                let end = find(&self.s[self.i..], b"?>")?;
                self.i += end + 2;
            } else if self.starts("<!") {
                return None;
            } else {
                return Some(());
            }
        }
    }

    fn name(&mut self) -> Option<String> {
        let st = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
        {
            self.i += 1;
        }
        (self.i > st).then(|| String::from_utf8_lossy(&self.s[st..self.i]).into_owned())
    }

    fn element(&mut self, depth: usize) -> Option<Element> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES || self.peek()? != b'<' {
            return None;
        }
        self.i += 1;
        let mut e = Element::new(&self.name()?);
        loop {
            self.skip_ws();
            match self.peek()? {
                b'/' => {
                    if self.s.get(self.i + 1) != Some(&b'>') {
                        return None;
                    }
                    self.i += 2;
                    return Some(e);
                }
                b'>' => {
                    self.i += 1;
                    break;
                }
                _ => {
                    let k = self.name()?;
                    self.skip_ws();
                    if self.peek()? != b'=' {
                        return None;
                    }
                    self.i += 1;
                    self.skip_ws();
                    let q = self.peek()?;
                    if q != b'"' && q != b'\'' {
                        return None;
                    }
                    self.i += 1;
                    let end = self.s[self.i..].iter().position(|&c| c == q)?;
                    let raw = std::str::from_utf8(&self.s[self.i..self.i + end]).ok()?;
                    self.nodes += 1;
                    if e.attrs.len() >= MAX_ATTRS
                        || self.nodes > MAX_NODES
                        || e.attrs.iter().any(|(n, _)| *n == k)
                    {
                        return None; // too many, or a repeated name (not well-formed)
                    }
                    e.attrs.push((k, crate::control::unescape(raw)?));
                    self.i += end + 1;
                }
            }
        }
        // Content: text and child elements until the matching end tag.
        loop {
            let st = self.i;
            while self.peek().is_some_and(|c| c != b'<') {
                self.i += 1;
            }
            if self.i > st {
                let raw = std::str::from_utf8(&self.s[st..self.i]).ok()?;
                e.text.push_str(&crate::control::unescape(raw)?);
            }
            if self.starts("</") {
                self.i += 2;
                if self.name()? != e.name {
                    return None;
                }
                self.skip_ws();
                if self.peek()? != b'>' {
                    return None;
                }
                self.i += 1;
                if e.text.trim().is_empty() && !e.children.is_empty() {
                    e.text.clear();
                }
                return Some(e);
            }
            // One comment or processing instruction, then on with the content (which may
            // be CDATA next: R1-54).
            if self.starts("<!--") {
                self.i += find(&self.s[self.i..], b"-->")? + 3;
                continue;
            }
            if self.starts("<?") {
                self.i += find(&self.s[self.i..], b"?>")? + 2;
                continue;
            }
            if self.starts("<![CDATA[") {
                let end = find(&self.s[self.i..], b"]]>")?;
                e.text
                    .push_str(std::str::from_utf8(&self.s[self.i + 9..self.i + end]).ok()?);
                self.i += end + 3;
                continue;
            }
            let c = self.element(depth + 1)?;
            e.children.push(c);
        }
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_before_cdata_and_attribute_floods() {
        assert_eq!(
            parse("<a><!-- x --><![CDATA[y]]><?pi z?>w</a>")
                .unwrap()
                .text,
            "yw"
        );
        let flood = format!(
            "<a {}/>",
            (0..300)
                .map(|i| format!("x{i}=\"\""))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(parse(&flood).is_none());
        assert!(
            parse(r#"<a x="1" x="2"/>"#).is_none(),
            "a repeated attribute"
        );
        assert_eq!(parse(r#"<a x="1" y="2"/>"#).unwrap().attrs.len(), 2);
    }

    #[test]
    fn nested_and_fragment() {
        let v = parse_fragment("<query/><receiver/><advertiser/>").unwrap();
        assert_eq!(
            v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["query", "receiver", "advertiser"]
        );
        let e = parse(r#"<source><name>A &amp; B</name><metadata/><address>0.0.0.0</address><port>5961</port><groups><group>public</group><group>x</group></groups></source>"#).unwrap();
        assert_eq!(e.child_text("name"), "A & B");
        assert_eq!(
            e.child("groups")
                .unwrap()
                .children_named("group")
                .map(|g| g.text.as_str())
                .collect::<Vec<_>>(),
            ["public", "x"]
        );
        assert_eq!(parse(&e.to_xml()).unwrap(), e);
    }

    #[test]
    fn both_empty_forms() {
        assert_eq!(
            parse(r#"<property name="a" value="b"/>"#),
            parse(r#"<property name="a" value="b"></property>"#)
        );
    }

    #[test]
    fn rejects_bad() {
        assert!(parse("<a><b></a>").is_none());
        assert!(parse("<!DOCTYPE a [<!ENTITY x 'y'>]><a>&x;</a>").is_none());
        assert!(parse("<a>&x;</a>").is_none());
        assert!(parse(&"<a>".repeat(100)).is_none());
        assert!(parse("<a/><b/>").is_none());
    }
}
