//! Finding plugins: CLAP files and VST3 bundles in the standard folders of each system
//! (and any given), each opened to list what it holds. A plugin that crashes while being
//! listed takes this process down, not the desk: scanning runs in a host process of its
//! own (`idn-plugin-host --scan`, started by `idn_plugin_host::scan`).

use idn_plugin_host::{clap_dirs, vst3_dirs};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Files under `dir` (a few levels down) with extension `ext`; a `.vst3` bundle is a
/// folder and isn't looked into.
fn find(dir: &Path, ext: &str, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
            out.push(p);
        } else if depth > 0 && p.is_dir() {
            find(&p, ext, depth - 1, out);
        }
    }
}

/// `--scan [PATH]...`: every plugin found, with the folders or files given too, as lines of
/// JSON on standard output: `{"trying": PATH}` before each, `{"entry": …}` for what it
/// holds, `{"done": true}` at the end. Standard input lists paths to leave out (one per
/// line): those already listed, and one that crashed or hung a scan before.
pub fn run(extra: &[String], out: std::fs::File) {
    let skip: HashSet<String> = std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.is_empty())
        .collect();
    let (mut claps, mut vst3s) = (Vec::new(), Vec::new());
    for d in clap_dirs() {
        find(&d, "clap", 3, &mut claps);
    }
    for d in vst3_dirs() {
        find(&d, "vst3", 3, &mut vst3s);
    }
    for e in extra {
        let p = PathBuf::from(e);
        match p.extension().and_then(|x| x.to_str()) {
            Some("clap") => claps.push(p),
            Some("vst3") => vst3s.push(p),
            _ => {
                find(&p, "clap", 3, &mut claps);
                find(&p, "vst3", 3, &mut vst3s);
            }
        }
    }
    let mut out = out;
    let mut say = |v: Value| {
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    };
    for (format, p) in claps
        .into_iter()
        .map(|p| ("clap", p))
        .chain(vst3s.into_iter().map(|p| ("vst3", p)))
    {
        let path = p.to_string_lossy().into_owned();
        if skip.contains(&path) {
            continue;
        }
        say(json!({"trying": path}));
        let listed = if format == "clap" {
            crate::clap::describe(&path)
        } else {
            crate::vst3::describe(&path)
        };
        match listed {
            Ok(list) => {
                for (id, name, vendor) in list {
                    say(
                        json!({"entry": {"key": format!("{format}:{path}#{id}"), "format": format,
                                         "name": name, "vendor": vendor, "path": path}}),
                    );
                }
            }
            Err(e) => say(json!({"entry": {"format": format, "path": path, "error": e}})),
        }
    }
    say(json!({"done": true}));
}
