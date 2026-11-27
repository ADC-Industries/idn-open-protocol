//! Makes the user guide (`docs/guide/*.md`) into static HTML pages: one a chapter, with
//! the chapters down the side, light and dark, printable. Opened from the disk, with no
//! server or internet connection; the installer puts them beside the programs (G7).
//!
//! ```text
//! idn-guide [GUIDE-DIR (docs/guide)] [OUT-DIR (GUIDE-DIR/html)]
//! ```
//!
//! Links between chapters (`02-quick-starts.md#live-captions`) become links between the
//! pages; a link to a chapter or a heading that isn't there stops it with the reason.

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const STYLE: &str = include_str!("guide.css");

/// One chapter: its file, its page, its title and its headings' anchors.
struct Chapter {
    file: String,
    page: String,
    title: String,
    anchors: BTreeSet<String>,
    body: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a.starts_with('-')) {
        eprintln!("usage: idn-guide [GUIDE-DIR (docs/guide)] [OUT-DIR (GUIDE-DIR/html)]");
        return ExitCode::from(2);
    }
    let dir = PathBuf::from(args.first().map_or("docs/guide", String::as_str));
    let out = args.get(1).map_or_else(|| dir.join("html"), PathBuf::from);
    match make(&dir, &out) {
        Ok(n) => {
            println!("idn-guide: {n} pages in {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("idn-guide: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The chapters in order: README.md first (the front page), then by file name.
fn chapters(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort_by_key(|p| (p.file_name().is_none_or(|n| n != "README.md"), p.clone()));
    if files.is_empty() {
        return Err(format!("{}: no chapters (.md)", dir.display()));
    }
    Ok(files)
}

fn page_of(file: &str) -> String {
    if file == "README.md" {
        "index.html".into()
    } else {
        file.trim_end_matches(".md").to_owned() + ".html"
    }
}

fn make(dir: &Path, out: &Path) -> Result<usize, String> {
    let mut book = Vec::new();
    // Every chapter's links, checked once all are read: (chapter, file, anchor).
    let mut links: Vec<(String, String, Option<String>)> = Vec::new();
    for path in chapters(dir)? {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
        let (title, anchors, body, found) = render(&text);
        links.extend(found.into_iter().map(|(f, a)| {
            let f = if f.is_empty() { file.clone() } else { f };
            (file.clone(), f, a)
        }));
        book.push(Chapter {
            page: page_of(&file),
            file,
            title: title.unwrap_or_else(|| "IDN".into()),
            anchors,
            body,
        });
    }
    let known: BTreeMap<&str, &Chapter> = book.iter().map(|c| (c.file.as_str(), c)).collect();
    let broken: Vec<String> = links
        .iter()
        .filter_map(|(from, to, anchor)| match known.get(to.as_str()) {
            None => Some(format!("{from}: a link to {to}, which isn't a chapter")),
            Some(c) => anchor
                .as_ref()
                .filter(|a| !c.anchors.contains(*a))
                .map(|a| format!("{from}: a link to {to}#{a}, which has no such heading")),
        })
        .collect();
    if !broken.is_empty() {
        return Err(broken.join("\n"));
    }
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    for c in &book {
        let nav: String = book
            .iter()
            .map(|x| {
                let here = if x.page == c.page {
                    " aria-current=\"page\""
                } else {
                    ""
                };
                format!(
                    "<li><a href=\"{}\"{here}>{}</a></li>",
                    x.page,
                    escape(&x.title)
                )
            })
            .collect();
        let page = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{title} · IDN guide</title>\n<style>\n{STYLE}</style>\n</head>\n<body>\n\
             <nav><a class=\"home\" href=\"index.html\">IDN guide</a><ol>{nav}</ol></nav>\n\
             <main>\n{body}</main>\n</body>\n</html>\n",
            title = escape(&c.title),
            body = c.body,
        );
        let to = out.join(&c.page);
        std::fs::write(&to, page).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    Ok(book.len())
}

/// A chapter's HTML: its first heading (the title), its headings' anchors, the body, and
/// its links to chapters (file, anchor; an empty file: this chapter).
type Rendered = (
    Option<String>,
    BTreeSet<String>,
    String,
    Vec<(String, Option<String>)>,
);

fn render(text: &str) -> Rendered {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    let mut title = None;
    let mut anchors = BTreeSet::new();
    let mut links = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    // A heading's events, held until its end: its anchor comes from its text.
    let mut heading: Option<(HeadingLevel, Vec<Event>, String)> = None;
    for ev in Parser::new_ext(text, opts) {
        let ev = match ev {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest = chapter_link(&dest_url).map_or(dest_url.clone(), |(file, anchor)| {
                    let to = if file.is_empty() {
                        String::new()
                    } else {
                        page_of(&file)
                    };
                    let url = match &anchor {
                        Some(a) => format!("{to}#{a}"),
                        None => to,
                    };
                    links.push((file, anchor));
                    CowStr::from(url)
                });
                Event::Start(Tag::Link {
                    link_type,
                    dest_url: dest,
                    title,
                    id,
                })
            }
            ev => ev,
        };
        match (&mut heading, ev) {
            (None, Event::Start(Tag::Heading { level, .. })) => {
                heading = Some((level, Vec::new(), String::new()));
            }
            (Some(_), Event::End(TagEnd::Heading(_))) => {
                let (level, inner, words) = heading.take().unwrap();
                let mut slug = slug(&words);
                // The same heading twice: GitHub's -1, -2 after the first.
                let base = slug.clone();
                let mut n = 1;
                while anchors.contains(&slug) {
                    slug = format!("{base}-{n}");
                    n += 1;
                }
                anchors.insert(slug.clone());
                if title.is_none() {
                    title = Some(words.clone());
                }
                let mut inside = String::new();
                html::push_html(&mut inside, inner.into_iter());
                let h = level as usize;
                events.push(Event::Html(CowStr::from(format!(
                    "<h{h} id=\"{slug}\">{inside}</h{h}>\n"
                ))));
            }
            (Some((_, inner, words)), ev) => {
                if let Event::Text(t) | Event::Code(t) = &ev {
                    words.push_str(t);
                }
                inner.push(ev);
            }
            (None, ev) => events.push(ev),
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    (title, anchors, body, links)
}

/// A link to a chapter (`X.md`, `X.md#a`) or a heading of this one (`#a`): (file, anchor).
/// Other links (addresses, files) are left as they are.
fn chapter_link(url: &str) -> Option<(String, Option<String>)> {
    if url.contains("://") || url.starts_with("mailto:") {
        return None;
    }
    let (file, anchor) = match url.split_once('#') {
        Some((f, a)) => (f, Some(a.to_owned())),
        None => (url, None),
    };
    (file.is_empty() || file.ends_with(".md")).then(|| (file.to_owned(), anchor))
}

/// A heading's anchor as GitHub makes it: lower case, spaces to hyphens, other marks gone.
fn slug(words: &str) -> String {
    words
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn escape(s: &str) -> String {
    idn_common::html::escape(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_get_githubs_anchors_and_links_go_to_pages() {
        assert_eq!(
            slug("A presenter's laptop or a webcam"),
            "a-presenters-laptop-or-a-webcam"
        );
        assert_eq!(
            slug("Ready for Sunday: save it and hand it to volunteers"),
            "ready-for-sunday-save-it-and-hand-it-to-volunteers"
        );
        let (title, anchors, body, links) = render(
            "# The mixer\n\n## Setup\n\n## Setup\n\nSee [captions](02-quick-starts.md#live-captions), \
             [here](#setup), [the guide](README.md) and <https://example.org/>.\n",
        );
        assert_eq!(title.as_deref(), Some("The mixer"));
        assert!(anchors.contains("setup") && anchors.contains("setup-1"));
        assert!(body.contains("<h2 id=\"setup-1\">Setup</h2>"));
        assert!(body.contains("href=\"02-quick-starts.html#live-captions\""));
        assert!(body.contains("href=\"#setup\"") && body.contains("href=\"index.html\""));
        assert!(body.contains("href=\"https://example.org/\""));
        assert_eq!(links.len(), 3);
    }

    #[test]
    fn the_guide_has_no_broken_links() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide");
        let out = idn_common::testing::TempDir::new("idn-guide-test");
        let n = make(&dir, &out).unwrap();
        assert!(n >= 2 && out.join("index.html").exists());
    }
}
