//! Data for titles (spec/extensions/idn-switcher.md §S.20): rows whose columns fill a
//! title's `{Column}` placeholders, a row at a time. From an IDN Data feed by name (its
//! text-only source, R.22: IDN Data reads the sheet or address and the switcher only
//! receives), or, as titles made before, read by the switcher itself from a file or an
//! address. The reading and the formats are IDN Data's, shared ([`idn_feeds`]).

pub use idn_feeds::{Format, MAX_ROWS, Row, parse, sheets_csv};
use serde_json::{Value, json};

/// A title's data: where from, how often it is read again, which row shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// An IDN Data feed's source ("HOST (Data: Scores)"); when set, `from`, `format` and
    /// `every` are IDN Data's.
    pub feed: String,
    /// A file on this machine, or an `http://` / `https://` address (a Google Sheets link
    /// is read as its CSV export: share the sheet with anyone with the link).
    pub from: String,
    pub format: Format,
    /// Seconds between reads (5–86400).
    pub every: f32,
    /// Seconds each row shows before the next (0: by hand).
    pub advance: f32,
}

impl Source {
    /// From the API's JSON: `{"feed"}` or `{"from", "format", "every"}`, and `advance`.
    pub fn from_json(v: &Value) -> Result<Source, String> {
        let s = |k: &str| v[k].as_str().map(str::trim).unwrap_or("").to_owned();
        let (feed, from) = (s("feed"), s("from"));
        if feed.is_empty() && from.is_empty() {
            return Err("data: an IDN Data feed (feed), or a file or an address (from)".into());
        }
        let num = |k: &str, d: f64, lo: f64, hi: f64| -> Result<f32, String> {
            match &v[k] {
                Value::Null => Ok(d as f32),
                x => x
                    .as_f64()
                    .filter(|x| x.is_finite())
                    .map(|x| x.clamp(lo, hi) as f32)
                    .ok_or(format!("data {k}: seconds")),
            }
        };
        Ok(Source {
            from: if feed.is_empty() { from } else { String::new() },
            feed,
            format: match &v["format"] {
                Value::Null => Format::Auto,
                f => Format::from_name(f.as_str().unwrap_or(""))
                    .ok_or("data format: auto, csv, tsv, json or xml")?,
            },
            every: num("every", 30.0, 5.0, 86_400.0)?,
            advance: num("advance", 0.0, 0.0, 86_400.0)?,
        })
    }

    pub fn to_json(&self) -> Value {
        if self.feed.is_empty() {
            json!({"from": self.from, "format": self.format.name(), "every": self.every,
                   "advance": self.advance})
        } else {
            json!({"feed": self.feed, "advance": self.advance})
        }
    }

    /// Where it is actually read from (a Google Sheets link as its CSV export).
    pub fn address(&self) -> String {
        sheets_csv(&self.from).unwrap_or_else(|| self.from.clone())
    }

    /// Reads it now (a file or an address; a feed is received instead): its rows.
    pub fn read(&self) -> Result<Vec<Row>, String> {
        idn_feeds::read(&self.from, self.format).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_or_an_address() {
        let f = Source::from_json(&json!({"feed": "OFFICE (Data: Scores)", "advance": 5})).unwrap();
        assert_eq!(
            (f.feed.as_str(), f.from.as_str(), f.advance),
            ("OFFICE (Data: Scores)", "", 5.0)
        );
        assert_eq!(Source::from_json(&f.to_json()).unwrap(), f);
        let a = Source::from_json(&json!({"from": "scores.csv", "every": 60})).unwrap();
        assert_eq!(Source::from_json(&a.to_json()).unwrap(), a);
        assert!(Source::from_json(&json!({"advance": 3})).is_err());
    }
}
