//! Rows to items and messages: ISO times, grouping, chunks and the envelope.

use super::*;

// ---------------------------------------------------------------------------------------
// Rows to items and messages.

/// A date, time or date-and-time as the driver writes it (`2026-11-04 07:12:03.1230000`),
/// as ISO 8601 local time (§I.10.6.2): a `T` between date and time, the fraction's
/// trailing zeros (and a fraction of zeros) left out: `2026-11-04T07:12:03.123`, a date
/// `2026-11-04`, a time `07:12:03`. Anything else (an offset, a driver's other form) as it
/// came.
pub fn iso_local(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    let is_date = |x: &[u8]| {
        x.len() == 10
            && x[4] == b'-'
            && x[7] == b'-'
            && x.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    };
    let is_time = |x: &str| {
        let (hms, frac) = x.split_once('.').unwrap_or((x, ""));
        let hb = hms.as_bytes();
        hb.len() == 8
            && hb[2] == b':'
            && hb[5] == b':'
            && hb
                .iter()
                .enumerate()
                .all(|(i, c)| i == 2 || i == 5 || c.is_ascii_digit())
            && frac.bytes().all(|c| c.is_ascii_digit())
    };
    let trim_frac = |x: &str| -> String {
        match x.split_once('.') {
            Some((hms, frac)) => {
                let f = frac.trim_end_matches('0');
                if f.is_empty() {
                    hms.to_owned()
                } else {
                    format!("{hms}.{f}")
                }
            }
            None => x.to_owned(),
        }
    };
    if is_date(b) {
        return t.to_owned();
    }
    if is_time(t) {
        return trim_frac(t);
    }
    if b.len() > 11 && is_date(&b[..10]) && (b[10] == b' ' || b[10] == b'T') && is_time(&t[11..]) {
        return format!("{}T{}", &t[..10], trim_frac(&t[11..]));
    }
    s.to_owned()
}

/// Rows as objects keyed by column.
pub fn objects(t: &Table) -> Vec<Map<String, Json>> {
    t.rows
        .iter()
        .map(|r| {
            t.columns
                .iter()
                .cloned()
                .zip(r.iter().cloned())
                .collect::<Map<String, Json>>()
        })
        .collect()
}

pub(super) fn key_of(o: &Map<String, Json>, cols: &[String]) -> String {
    let k: Vec<&Json> = cols
        .iter()
        .map(|c| o.get(c).unwrap_or(&Json::Null))
        .collect();
    serde_json::to_string(&k).unwrap_or_default()
}

/// Rows sharing the `group_by` columns become one item, in the order they first appear:
/// the group's columns (and any not in `lines`, from its first row) and a `lines` array
/// of the `lines` columns. A line whose columns are all null (a left join with nothing)
/// is left out. Without `group_by` each row is an item.
pub fn group(rows: Vec<Map<String, Json>>, group_by: &[String], lines: &[String]) -> Vec<Json> {
    if group_by.is_empty() {
        return rows.into_iter().map(Json::Object).collect();
    }
    let mut order: Vec<String> = Vec::new();
    let mut items: HashMap<String, Map<String, Json>> = HashMap::new();
    for r in rows {
        let k = key_of(&r, group_by);
        let (head, line): (Map<String, Json>, Map<String, Json>) = if lines.is_empty() {
            r.into_iter().partition(|(c, _)| group_by.contains(c))
        } else {
            r.into_iter().partition(|(c, _)| !lines.contains(c))
        };
        let item = items.entry(k.clone()).or_insert_with(|| {
            order.push(k);
            let mut h = head;
            h.insert("lines".into(), Json::Array(Vec::new()));
            h
        });
        if line.values().any(|v| !v.is_null())
            && let Some(Json::Array(a)) = item.get_mut("lines")
        {
            a.push(Json::Object(line));
        }
    }
    order
        .into_iter()
        .filter_map(|k| items.remove(&k).map(Json::Object))
        .collect()
}

/// Items in chunks of at most `max_items` and about `max_bytes` (a single larger item
/// goes alone).
pub fn chunks(items: Vec<Json>, max_items: usize, max_bytes: usize) -> Vec<Vec<Json>> {
    // Room for the envelope around the items.
    let budget = max_bytes.saturating_sub(1024).max(1024);
    let mut out: Vec<Vec<Json>> = Vec::new();
    let mut cur: Vec<Json> = Vec::new();
    let mut size = 0;
    for it in items {
        let n = serde_json::to_vec(&it).map_or(0, |v| v.len()) + 1;
        if !cur.is_empty() && (cur.len() >= max_items || size + n > budget) {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += n;
        cur.push(it);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A message's envelope: what it is, so the application knows what absence means.
#[allow(clippy::too_many_arguments)]
pub(super) fn envelope(
    q: &Query,
    zone: &Option<String>,
    observed_ms: i64,
    mode: &str,
    items: Vec<Json>,
    part: usize,
    parts: usize,
    set: Option<&str>,
    warnings: &[String],
) -> Json {
    let mut m = serde_json::json!({
        "type": q.message,
        "version": q.version,
        "query": q.name,
        "mode": mode,
        "zone": zone,
        "observed_ms": observed_ms,
        "part": part,
        "parts": parts,
        "items": items,
    });
    if let Some(s) = set {
        m["set"] = Json::String(s.to_owned());
    }
    if !warnings.is_empty() {
        m["warnings"] = serde_json::json!(warnings);
    }
    m
}
