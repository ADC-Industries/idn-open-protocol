//! A cursor query's place: its file, one run at a time, and the furthest row sent.

use super::*;

pub(super) fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn rand_u64() -> u64 {
    u64::from_le_bytes(idn_open_protocol::random::secure_random())
}

#[derive(Serialize, Deserialize)]
struct CursorFile {
    columns: Vec<String>,
    values: Vec<Json>,
}

/// Where the cursor is: from its file, else (none yet, or the query's cursor columns
/// changed) from the query's start. A file that is there but can't be read is an error:
/// starting over sent the whole history again, and the next save wrote over where it
/// really was (bug review 3).
pub(super) fn load_cursor(path: &Path, q: &Query) -> Result<Vec<Json>, String> {
    let start = || {
        if q.start.len() == q.cursor.len() {
            q.start.clone()
        } else {
            vec![Json::Null; q.cursor.len()]
        }
    };
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(start()),
        Err(e) => return Err(format!("its cursor ({}): {e}", path.display())),
    };
    match serde_json::from_slice::<CursorFile>(&b) {
        Ok(c) if c.columns == q.cursor => Ok(c.values),
        Ok(_) => Ok(start()),
        Err(e) => Err(format!(
            "its cursor ({}) can't be read: {e}",
            path.display()
        )),
    }
}

pub(super) fn save_cursor(path: &Path, q: &Query, values: &[Json]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = CursorFile {
        columns: q.cursor.clone(),
        values: values.to_vec(),
    };
    idn_common::fsutil::write_atomic(path, &serde_json::to_vec(&f).unwrap_or_default())
}

/// A cursor's run in progress (in this process), by its file.
pub(super) struct CursorRun(PathBuf);

static CURSOR_RUNS: std::sync::LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

impl CursorRun {
    pub(super) fn take(file: &Path) -> Option<CursorRun> {
        CURSOR_RUNS
            .lock_ok()
            .insert(file.to_path_buf())
            .then(|| CursorRun(file.to_path_buf()))
    }
}

impl Drop for CursorRun {
    fn drop(&mut self) {
        CURSOR_RUNS.lock_ok().remove(&self.0);
    }
}

/// The furthest cursor among all the rows of `items` (each item's lines, or the item
/// itself), compared column by column: numbers as numbers, text as text (ISO times sort
/// so). `None` with no items.
pub(super) fn furthest_cursor(items: &[Json], q: &Query) -> Option<Vec<Json>> {
    fn cmp(a: &Json, b: &Json) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        match (a, b) {
            (Json::Number(x), Json::Number(y)) => {
                x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Equal)
            }
            (Json::String(x), Json::String(y)) => x.cmp(y),
            (Json::Null, Json::Null) => Equal,
            (Json::Null, _) => std::cmp::Ordering::Less,
            (_, Json::Null) => std::cmp::Ordering::Greater,
            _ => Equal,
        }
    }
    let mut best: Option<Vec<Json>> = None;
    for it in items {
        let Some(o) = it.as_object() else { continue };
        let lines: Vec<&Map<String, Json>> = o
            .get("lines")
            .and_then(|l| l.as_array())
            .map(|l| l.iter().filter_map(|x| x.as_object()).collect())
            .unwrap_or_default();
        let rows: Vec<Option<&Map<String, Json>>> = if lines.is_empty() {
            vec![None]
        } else {
            lines.into_iter().map(Some).collect()
        };
        for line in rows {
            let c: Vec<Json> = q
                .cursor
                .iter()
                .map(|k| {
                    o.get(k)
                        .or_else(|| line.and_then(|l| l.get(k)))
                        .cloned()
                        .unwrap_or(Json::Null)
                })
                .collect();
            let further = best.as_ref().is_none_or(|b| {
                c.iter()
                    .zip(b)
                    .map(|(x, y)| cmp(x, y))
                    .find(|o| o.is_ne())
                    .is_some_and(|o| o.is_gt())
            });
            if further {
                best = Some(c);
            }
        }
    }
    best
}

/// The cursor columns' values in the chunk's last item (its last line, if grouped and the
/// columns are line columns).
pub(super) fn last_cursor(chunk: &[Json], q: &Query) -> Option<Vec<Json>> {
    let it = chunk.last()?.as_object()?;
    let line = it
        .get("lines")
        .and_then(|l| l.as_array())
        .and_then(|l| l.last())
        .and_then(|l| l.as_object());
    Some(
        q.cursor
            .iter()
            .map(|c| {
                it.get(c)
                    .or_else(|| line.and_then(|l| l.get(c)))
                    .cloned()
                    .unwrap_or(Json::Null)
            })
            .collect(),
    )
}
