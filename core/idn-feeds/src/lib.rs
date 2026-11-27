//! Reading data (roadmap R.22; first the switcher's data titles, idn-switcher.md §S.20):
//! rows from a file or an address — CSV or TSV (a Google Sheet's export), JSON, XML or an
//! RSS feed. IDN Data reads each feed every so often and sends its rows as a text-only
//! source ([`idn_open_protocol::text`]); the switcher's titles fill their `{Column}`
//! placeholders from them, a row at a time.

use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

/// Rows a source gives, at most (the rest are left out).
pub const MAX_ROWS: usize = 10_000;
/// Columns read from a CSV or TSV (the rest left out).
pub const MAX_COLUMNS: usize = 256;

/// A row: its values by column name.
pub use idn_open_protocol::text::Row;

/// What a source is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// By the address's ending, else by its first character.
    Auto,
    Csv,
    Tsv,
    Json,
    /// XML: an RSS or Atom feed's items, else the root's repeated children.
    Xml,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Xml => "xml",
        }
    }

    pub fn from_name(s: &str) -> Option<Format> {
        Some(match s {
            "auto" => Format::Auto,
            "csv" => Format::Csv,
            "tsv" => Format::Tsv,
            "json" => Format::Json,
            "xml" | "rss" => Format::Xml,
            _ => return None,
        })
    }
}

/// Reads `from` now — a file on this machine, or an `http://` / `https://` address (a
/// Google Sheets link is read as its CSV export: share the sheet with anyone with the
/// link) — as rows.
pub fn read(from: &str, format: Format) -> std::io::Result<Vec<Row>> {
    read_table(from, format).map(|t| t.0)
}

/// As [`read`], with the columns in the source's order where it has one (a CSV's or TSV's
/// header; empty for JSON and XML, whose rows keep name order).
/// (A file or address that can't be read, or isn't in the format: an error whose text is
/// what to tell the person.)
pub fn read_table(from: &str, format: Format) -> std::io::Result<(Vec<Row>, Vec<String>)> {
    let at = sheets_csv(from).unwrap_or_else(|| from.to_owned());
    let body = if at.starts_with("http://") || at.starts_with("https://") {
        idn_open_protocol::web::httpc::get(&at, Duration::from_secs(10))?
    } else {
        read_file(&at).map_err(|e| std::io::Error::new(e.kind(), format!("{at}: {e}")))?
    };
    let text = String::from_utf8_lossy(&body);
    let format = match format {
        Format::Auto if at.contains("format=csv") => Format::Csv,
        f => f,
    };
    parse_table(&text, format, &at)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The most of a file read as a table (far more than any titles' data).
const MAX_FILE: u64 = 16 << 20;

/// A plain file, at most [`MAX_FILE`]: read whole, a device or a pipe named as the source
/// (`/dev/zero`) filled memory without end (deep bug run 2026-11-09).
fn read_file(path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a file",
        ));
    }
    let mut b = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut b)?;
    if b.len() as u64 > MAX_FILE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "over 16 MB: too large for titles' data",
        ));
    }
    Ok(b)
}

/// A Google Sheets link (`…/spreadsheets/d/ID/edit#gid=N`) as its CSV export's address.
pub fn sheets_csv(url: &str) -> Option<String> {
    let rest = url.split("docs.google.com/spreadsheets/d/").nth(1)?;
    let id = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|i| !i.is_empty())?;
    if id == "e" {
        // A "published to the web" link: its own CSV form.
        let pub_id = rest.split('/').nth(1)?;
        return Some(format!(
            "https://docs.google.com/spreadsheets/d/e/{pub_id}/pub?output=csv"
        ));
    }
    let gid = url
        .split("gid=")
        .nth(1)
        .map(|g| {
            g.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "0".into());
    Some(format!(
        "https://docs.google.com/spreadsheets/d/{id}/export?format=csv&gid={gid}"
    ))
}

/// Text in a format as rows (`name` helps guess the format).
pub fn parse(text: &str, format: Format, name: &str) -> Result<Vec<Row>, String> {
    parse_table(text, format, name).map(|t| t.0)
}

/// As [`parse`], with the columns in the source's order where it has one.
pub fn parse_table(
    text: &str,
    format: Format,
    name: &str,
) -> Result<(Vec<Row>, Vec<String>), String> {
    let text = text.trim_start_matches('\u{feff}');
    let format = match format {
        Format::Auto => {
            let lower = name.to_ascii_lowercase();
            let first = text.trim_start().chars().next().unwrap_or(' ');
            if lower.ends_with(".json") || first == '[' || first == '{' {
                Format::Json
            } else if lower.ends_with(".xml") || lower.ends_with(".rss") || first == '<' {
                Format::Xml
            } else if lower.ends_with(".tsv")
                || text
                    .lines()
                    .next()
                    .is_some_and(|l| l.contains('\t') && !l.contains(','))
            {
                Format::Tsv
            } else {
                Format::Csv
            }
        }
        f => f,
    };
    let (mut rows, columns) = match format {
        Format::Csv => delimited(text, ','),
        Format::Tsv => delimited(text, '\t'),
        Format::Json => (json_rows(text)?, Vec::new()),
        Format::Xml => (xml_rows(text)?, Vec::new()),
        Format::Auto => unreachable!(),
    };
    rows.truncate(MAX_ROWS);
    Ok((rows, columns))
}

/// CSV (RFC 4180: quotes, doubled quotes, line breaks in quotes) or TSV: the first line
/// names the columns (an empty name is `colN`); the rows and the columns in order.
fn delimited(text: &str, sep: char) -> (Vec<Row>, Vec<String>) {
    let mut records: Vec<Vec<String>> = Vec::new();
    let (mut rec, mut field, mut quoted, mut was_quoted) =
        (Vec::new(), String::new(), false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            c if c == sep => {
                rec.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' => {}
            '\n' => {
                rec.push(std::mem::take(&mut field));
                was_quoted = false;
                records.push(std::mem::take(&mut rec));
                if records.len() > MAX_ROWS {
                    break;
                }
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !rec.is_empty() {
        rec.push(field);
        records.push(rec);
    }
    let mut it = records
        .into_iter()
        .filter(|r| r.iter().any(|f| !f.is_empty()));
    let Some(head) = it.next() else {
        return (Vec::new(), Vec::new());
    };
    // At most MAX_COLUMNS columns, names of at most 64 characters: every row is filled to
    // every column, and a header of a million commas made rows × columns entries enough
    // to run the program out of memory (bug round E, E4-1).
    let names: Vec<String> = head
        .iter()
        .take(MAX_COLUMNS)
        .enumerate()
        .map(|(i, h)| {
            let h = h.trim();
            if h.is_empty() {
                format!("col{}", i + 1)
            } else {
                h.chars().take(64).collect()
            }
        })
        .collect();
    let rows = it
        .map(|r| {
            names
                .iter()
                .cloned()
                .zip(r.into_iter().chain(std::iter::repeat(String::new())))
                .collect()
        })
        .collect();
    (rows, names)
}

/// JSON: an array of objects, or the first such array in an object (`{"items": [...]}`);
/// nested values by dotted names (`team.name`), lists as text.
fn json_rows(text: &str) -> Result<Vec<Row>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("JSON: {e}"))?;
    fn find(v: &Value) -> Option<&Vec<Value>> {
        match v {
            Value::Array(a) if a.iter().any(Value::is_object) => Some(a),
            Value::Object(m) => m.values().find_map(find),
            _ => None,
        }
    }
    fn flat(prefix: &str, v: &Value, row: &mut Row) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    flat(&key, x, row);
                }
            }
            Value::String(s) => {
                row.insert(prefix.to_owned(), s.clone());
            }
            Value::Null => {
                row.insert(prefix.to_owned(), String::new());
            }
            x => {
                row.insert(prefix.to_owned(), x.to_string());
            }
        }
    }
    let items = match &v {
        Value::Object(_) if find(&v).is_none() => {
            return Ok(vec![{
                let mut r = Row::new();
                flat("", &v, &mut r);
                r
            }]);
        }
        _ => find(&v).ok_or("JSON: no list of objects")?,
    };
    Ok(items
        .iter()
        .filter(|x| x.is_object())
        .map(|x| {
            let mut r = Row::new();
            flat("", x, &mut r);
            r
        })
        .collect())
}

/// An XML element: its name, text and children (attributes as `@name` children).
#[derive(Debug, Default)]
struct Element {
    name: String,
    text: String,
    children: Vec<Element>,
}

/// XML's entities.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find(';').filter(|&j| j <= 10) {
            Some(j) => {
                let e = &after[..j];
                let c = match e {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => e
                        .strip_prefix("#x")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                match c {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[i..i + j + 2]),
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A small XML reader: elements, text, CDATA, attributes; comments, declarations and
/// processing instructions skipped. Enough for feeds and simple data files.
fn xml_tree(text: &str) -> Result<Element, String> {
    let mut stack = vec![Element::default()];
    let mut rest = text;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            stack.last_mut().unwrap().text.push_str(&unescape(rest));
            break;
        };
        stack
            .last_mut()
            .unwrap()
            .text
            .push_str(&unescape(&rest[..lt]));
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or("XML: an unclosed CDATA")?;
            stack.last_mut().unwrap().text.push_str(&r[..end]);
            rest = &r[end + 3..];
        } else if let Some(r) = rest.strip_prefix("<!--") {
            rest = &r[r.find("-->").ok_or("XML: an unclosed comment")? + 3..];
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = &rest[rest.find('>').ok_or("XML: an unclosed declaration")? + 1..];
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or("XML: an unclosed tag")?;
            rest = &r[end + 1..];
            if stack.len() > 1 {
                let done = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(done);
            }
        } else {
            let end = rest.find('>').ok_or("XML: an unclosed tag")?;
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            let selfclosing = tag.ends_with('/');
            // (Trimmed first: the name is sliced off the front, and whitespace before it,
            // Unicode's too, cut a character in two.)
            let tag = tag.trim_end_matches('/').trim();
            let name = tag.split_whitespace().next().unwrap_or("").to_owned();
            // Attributes: name="value" or name='value'.
            let mut attrs = tag.strip_prefix(name.as_str()).unwrap_or("").trim();
            let mut el = Element {
                name,
                ..Default::default()
            };
            while let Some(eq) = attrs.find('=') {
                let key = attrs[..eq].trim().to_owned();
                let v = attrs[eq + 1..].trim_start();
                let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                    break;
                };
                let Some(close) = v[1..].find(q) else {
                    break;
                };
                el.children.push(Element {
                    name: format!("@{key}"),
                    text: unescape(&v[1..1 + close]),
                    children: Vec::new(),
                });
                attrs = v[close + 2..].trim_start();
            }
            if selfclosing {
                stack.last_mut().unwrap().children.push(el);
            } else {
                stack.push(el);
            }
            if stack.len() > 64 {
                return Err("XML: nested too deep".into());
            }
        }
    }
    while stack.len() > 1 {
        let done = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(done);
    }
    Ok(stack.pop().unwrap())
}

/// XML as rows: an RSS feed's `item`s or an Atom feed's `entry`s, else the most repeated
/// element's; each row its children's text by name (a link's `@href` as `link`).
fn xml_rows(text: &str) -> Result<Vec<Row>, String> {
    let root = xml_tree(text)?;
    fn all<'a>(e: &'a Element, name: &str, out: &mut Vec<&'a Element>) {
        for c in &e.children {
            if c.name == name {
                out.push(c);
            } else {
                all(c, name, out);
            }
        }
    }
    let mut items = Vec::new();
    for n in ["item", "entry"] {
        all(&root, n, &mut items);
        if !items.is_empty() {
            break;
        }
    }
    if items.is_empty() {
        // The element with the most children of one name.
        fn best<'a>(e: &'a Element, found: &mut (usize, Vec<&'a Element>)) {
            let mut by: BTreeMap<&str, Vec<&Element>> = BTreeMap::new();
            for c in e.children.iter().filter(|c| !c.name.starts_with('@')) {
                by.entry(c.name.as_str()).or_default().push(c);
            }
            for (_, v) in by {
                if v.len() > found.0 {
                    *found = (v.len(), v);
                }
            }
            for c in &e.children {
                best(c, found);
            }
        }
        let mut found = (0, Vec::new());
        best(&root, &mut found);
        items = found.1;
    }
    Ok(items
        .into_iter()
        .map(|it| {
            let mut r = Row::new();
            for c in &it.children {
                let key = c.name.trim_start_matches('@').to_owned();
                let v = match c.text.trim() {
                    "" => c
                        .children
                        .iter()
                        .find(|a| a.name == "@href")
                        .map_or(String::new(), |a| a.text.clone()),
                    t => t.to_owned(),
                };
                r.entry(key).or_insert(v);
            }
            r
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CSV's columns come in its header's order (they came back sorted).
    #[test]
    fn columns_in_the_headers_order() {
        let (rows, columns) = parse_table("Team,Score\nLions,3\n", Format::Csv, "x.csv").unwrap();
        assert_eq!(columns, ["Team", "Score"]);
        let m = idn_open_protocol::text::DataMessage::rows_in_order(rows, &columns);
        assert_eq!(m.columns, ["Team", "Score"]);
    }

    #[test]
    fn csv_and_tsv() {
        let rows = parse(
            "Name,Role,Note\nSam Lee,Pastor,\"Says \"\"hi\"\", often\"\n\"Ana\nMaría\",Worship\n",
            Format::Auto,
            "people.csv",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["Name"], "Sam Lee");
        assert_eq!(rows[0]["Note"], "Says \"hi\", often");
        assert_eq!(rows[1]["Name"], "Ana\nMaría");
        assert_eq!(rows[1]["Note"], "");
        let rows = parse("a\tb\n1\t2\n", Format::Auto, "x").unwrap();
        assert_eq!((rows[0]["a"].as_str(), rows[0]["b"].as_str()), ("1", "2"));
    }

    #[test]
    fn json_lists_and_nesting() {
        let rows = parse(
            r#"{"meta": 1, "games": [{"home": {"name": "Eagles", "score": 3}, "live": true}, {"home": {"name": "Owls", "score": null}}]}"#,
            Format::Auto,
            "x",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["home.name"], "Eagles");
        assert_eq!(rows[0]["home.score"], "3");
        assert_eq!(rows[0]["live"], "true");
        assert_eq!(rows[1]["home.score"], "");
        // One object: one row.
        assert_eq!(
            parse(r#"{"verse": "John 3:16"}"#, Format::Json, "x").unwrap()[0]["verse"],
            "John 3:16"
        );
        assert!(parse("[1,", Format::Json, "x").is_err());
    }

    #[test]
    fn rss_atom_and_plain_xml() {
        let rss = r#"<?xml version="1.0"?><rss><channel><title>News</title>
            <item><title>Sunday &amp; more</title><link>https://a.org/1</link></item>
            <item><title><![CDATA[Youth <night>]]></title></item></channel></rss>"#;
        let rows = parse(rss, Format::Auto, "feed").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["title"], "Sunday & more");
        assert_eq!(rows[1]["title"], "Youth <night>");
        let atom = r#"<feed><entry><title>A</title><link href="https://b/1"/></entry></feed>"#;
        assert_eq!(
            parse(atom, Format::Xml, "x").unwrap()[0]["link"],
            "https://b/1"
        );
        let plain = r#"<songs><song id="7"><name>Amazing Grace</name><key>G</key></song>
            <song id="8"><name>Be Thou My Vision</name><key>D</key></song></songs>"#;
        let rows = parse(plain, Format::Auto, "x").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[1]["name"].as_str(), rows[1]["id"].as_str()),
            ("Be Thou My Vision", "8")
        );
    }

    #[test]
    fn google_sheets_links() {
        assert_eq!(
            sheets_csv("https://docs.google.com/spreadsheets/d/1AbC/edit#gid=42").unwrap(),
            "https://docs.google.com/spreadsheets/d/1AbC/export?format=csv&gid=42"
        );
        assert_eq!(
            sheets_csv("https://docs.google.com/spreadsheets/d/1AbC/edit?usp=sharing").unwrap(),
            "https://docs.google.com/spreadsheets/d/1AbC/export?format=csv&gid=0"
        );
        assert_eq!(
            sheets_csv("https://docs.google.com/spreadsheets/d/e/2PACX-x/pubhtml").unwrap(),
            "https://docs.google.com/spreadsheets/d/e/2PACX-x/pub?output=csv"
        );
        assert!(sheets_csv("https://example.org/a.csv").is_none());
    }
}

#[cfg(test)]
mod column_cap {
    #[test]
    fn a_header_of_many_columns_is_cut() {
        let mut t = format!("a{}", ",".repeat(100_000));
        t.push('\n');
        for _ in 0..100 {
            t.push_str("x\n");
        }
        let (rows, names) = super::delimited(&t, ',');
        assert_eq!(names.len(), super::MAX_COLUMNS);
        assert!(rows.iter().all(|r| r.len() == super::MAX_COLUMNS));
    }
}
