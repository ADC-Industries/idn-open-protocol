//! The worker's database side (§I.10.6.3): ODBC through the system's driver manager
//! (Windows' odbc32; unixODBC elsewhere with the `odbc` feature). Connections are kept
//! between jobs and dropped after any failure, so the next job connects afresh.
//!
//! Every query runs with autocommit off and is rolled back afterwards: even a statement
//! that got past the read-only check can't keep a change.

use super::worker::{Fail, Job, Table};

#[cfg(not(any(windows, feature = "odbc")))]
#[derive(Default)]
pub struct Db;

#[cfg(not(any(windows, feature = "odbc")))]
impl Db {
    pub fn run(&mut self, _job: &Job) -> Result<Table, Fail> {
        Err(Fail::Refused(
            "this build has no ODBC (build with the `odbc` feature)".into(),
        ))
    }
}

#[cfg(any(windows, feature = "odbc"))]
pub use imp::Db;

#[cfg(any(windows, feature = "odbc"))]
mod imp {
    use super::{Fail, Job, Table};
    use odbc_api::parameter::InputParameter;
    use odbc_api::{Connection, ConnectionOptions, Cursor, CursorRow, DataType, IntoParameter};
    use serde_json::Value as Json;
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct Db {
        conns: HashMap<String, Connection<'static>>,
    }

    fn one_line(e: &odbc_api::Error) -> String {
        e.to_string().replace(['\n', '\r'], " ")
    }

    /// The SQLSTATE says what kind of failure it is.
    fn classify(e: &odbc_api::Error) -> Fail {
        if let odbc_api::Error::Diagnostics { record, .. } = e {
            let st = record.state.as_str();
            if st.starts_with("08") {
                return Fail::Unavailable(one_line(e));
            }
            if matches!(st, "HYT00" | "HYT01" | "HY008") {
                return Fail::Timeout(one_line(e));
            }
        }
        Fail::Error(one_line(e))
    }

    impl Db {
        pub fn run(&mut self, job: &Job) -> Result<Table, Fail> {
            let t = Some(job.timeout_s as usize);
            if !self.conns.contains_key(&job.conn) {
                let env = odbc_api::environment().map_err(|e| Fail::Error(one_line(&e)))?;
                let c = env
                    .connect_with_connection_string(
                        &job.conn,
                        ConnectionOptions {
                            login_timeout_sec: Some(15.min(job.timeout_s as u32).max(1)),
                            ..Default::default()
                        },
                    )
                    .map_err(|e| Fail::Unavailable(one_line(&e)))?;
                for s in &job.init {
                    c.execute(s, (), t).map_err(|e| classify(&e))?;
                }
                self.conns.insert(job.conn.clone(), c);
            }
            let c = self.conns.get(&job.conn).unwrap();
            let r = query(c, job);
            if r.is_err() {
                self.conns.remove(&job.conn);
            }
            r
        }
    }

    fn param(v: &Json) -> Box<dyn InputParameter> {
        match v {
            Json::Null => Box::new(odbc_api::parameter::VarCharBox::null()),
            Json::Bool(b) => Box::new(*b as i64),
            Json::Number(n) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => Box::new(i),
                (None, Some(f)) => Box::new(f),
                _ => Box::new(n.to_string().into_parameter()),
            },
            Json::String(s) => text_param(s),
            other => text_param(&other.to_string()),
        }
    }

    #[cfg(windows)]
    fn text_param(s: &str) -> Box<dyn InputParameter> {
        Box::new(odbc_api::parameter::VarWCharBox::from_vec(
            s.encode_utf16().collect(),
        ))
    }

    #[cfg(not(windows))]
    fn text_param(s: &str) -> Box<dyn InputParameter> {
        Box::new(odbc_api::parameter::VarCharBox::from_string(s.to_owned()))
    }

    fn query(c: &Connection<'static>, job: &Job) -> Result<Table, Fail> {
        let t = Some(job.timeout_s as usize);
        c.set_autocommit(false).map_err(|e| classify(&e))?;
        let r = (|| {
            for s in &job.each {
                c.execute(s, (), t).map_err(|e| classify(&e))?;
            }
            let params: Vec<Box<dyn InputParameter>> = job.params.iter().map(param).collect();
            let Some(mut cursor) = c
                .execute(&job.sql, &params[..], t)
                .map_err(|e| classify(&e))?
            else {
                return Ok(Table::default());
            };
            fetch(&mut cursor, job.max_rows)
        })();
        // Never keep anything: a read leaves no change behind.
        let _ = c.rollback();
        r
    }

    fn fetch(cursor: &mut impl Cursor, max_rows: usize) -> Result<Table, Fail> {
        let n = cursor.num_result_cols().map_err(|e| classify(&e))? as u16;
        let mut t = Table::default();
        let mut types = Vec::new();
        for i in 1..=n {
            t.columns
                .push(cursor.col_name(i).map_err(|e| classify(&e))?);
            types.push(cursor.col_data_type(i).map_err(|e| classify(&e))?);
        }
        let mut bad_cols: Vec<String> = Vec::new();
        while let Some(mut row) = cursor.next_row().map_err(|e| classify(&e))? {
            if t.rows.len() >= max_rows {
                return Err(Fail::Error(format!(
                    "more than {max_rows} rows; narrow the query"
                )));
            }
            let mut r = Vec::with_capacity(n as usize);
            for (i, ty) in types.iter().enumerate() {
                let (v, ok) = cell(&mut row, i as u16 + 1, ty)?;
                if !ok && !bad_cols.contains(&t.columns[i]) {
                    bad_cols.push(t.columns[i].clone());
                }
                r.push(v);
            }
            t.rows.push(r);
        }
        for c in bad_cols {
            t.warnings.push(format!(
                "column \"{c}\": a value that isn't text was sent as null"
            ));
        }
        Ok(t)
    }

    #[cfg(windows)]
    fn text(row: &mut CursorRow<'_>, col: u16) -> Result<Option<(String, bool)>, Fail> {
        let mut b: Vec<u16> = Vec::new();
        if row.get_wide_text(col, &mut b).map_err(|e| classify(&e))? {
            Ok(Some(match String::from_utf16(&b) {
                Ok(s) => (s, true),
                Err(_) => (String::from_utf16_lossy(&b), false),
            }))
        } else {
            Ok(None)
        }
    }

    #[cfg(not(windows))]
    fn text(row: &mut CursorRow<'_>, col: u16) -> Result<Option<(String, bool)>, Fail> {
        let mut b: Vec<u8> = Vec::new();
        if row.get_text(col, &mut b).map_err(|e| classify(&e))? {
            Ok(Some(match String::from_utf8(b) {
                Ok(s) => (s, true),
                Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), false),
            }))
        } else {
            Ok(None)
        }
    }

    /// One value as JSON: numbers as numbers, bits as booleans, binary as base64, the rest
    /// (text, dates and times) as the driver gives them. `false`: it couldn't be read as it
    /// is (a null or a lossy text instead).
    fn cell(row: &mut CursorRow<'_>, col: u16, ty: &DataType) -> Result<(Json, bool), Fail> {
        match ty {
            DataType::Binary { .. }
            | DataType::Varbinary { .. }
            | DataType::LongVarbinary { .. } => {
                let mut b = Vec::new();
                return Ok(if row.get_binary(col, &mut b).map_err(|e| classify(&e))? {
                    (
                        Json::String(idn_common::text::base64(&b)),
                        true,
                    )
                } else {
                    (Json::Null, true)
                });
            }
            _ => {}
        }
        let Some((s, ok)) = text(row, col)? else {
            return Ok((Json::Null, true));
        };
        if !ok {
            return Ok((Json::Null, false));
        }
        let v = match ty {
            DataType::Integer | DataType::SmallInt | DataType::BigInt | DataType::TinyInt => s
                .trim()
                .parse::<i64>()
                .map(Json::from)
                .unwrap_or(Json::String(s)),
            DataType::Bit => match s.trim() {
                "1" => Json::Bool(true),
                "0" => Json::Bool(false),
                _ => Json::String(s),
            },
            DataType::Float { .. }
            | DataType::Real
            | DataType::Double
            | DataType::Numeric { .. }
            | DataType::Decimal { .. } => {
                let x = s.trim();
                if let Ok(i) = x.parse::<i64>() {
                    Json::from(i)
                } else {
                    x.parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map(Json::Number)
                        .unwrap_or(Json::String(s))
                }
            }
            // Dates and times as ISO 8601 local time, whatever the driver's own form.
            DataType::Date | DataType::Time { .. } | DataType::Timestamp { .. } => {
                Json::String(crate::sql::iso_local(&s))
            }
            _ => Json::String(s),
        };
        Ok((v, true))
    }
}
