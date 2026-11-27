//! The read-only check: one SELECT or WITH statement, and its `?` placeholders.

// ---------------------------------------------------------------------------------------
// The read-only check.

/// Words that can change data, the schema or the server, or reach outside the database.
const WRITES: &[&str] = &[
    // Statements that write, change the schema or the server, or run code (SQL Server's
    // words; not function or likely column names such as REPLACE or LOAD: the rollback
    // and the account's rights cover the rest).
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "INTO",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "DENY",
    "EXEC",
    "EXECUTE",
    "BACKUP",
    "RESTORE",
    "DBCC",
    "SHUTDOWN",
    "KILL",
    "RECONFIGURE",
    "BULK",
    "OPENROWSET",
    "OPENDATASOURCE",
    "OPENQUERY",
    "OPENXML",
    "WAITFOR",
    "USE",
    "SET",
    "DECLARE",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
];

/// Splits SQL into its words and punctuation outside strings, quoted names and comments.
fn tokens(sql: &str) -> (Vec<String>, Vec<char>) {
    let mut words = Vec::new();
    let mut punct = Vec::new();
    let c: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut word = String::new();
    let flush = |w: &mut String, words: &mut Vec<String>| {
        if !w.is_empty() {
            words.push(w.to_ascii_uppercase());
            w.clear();
        }
    };
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            '\'' | '"' | '[' | '`' => {
                flush(&mut word, &mut words);
                let close = if ch == '[' { ']' } else { ch };
                i += 1;
                while i < c.len() {
                    if c[i] == close {
                        // A doubled quote is one quote inside.
                        if c.get(i + 1) == Some(&close) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                // A string stands in the token stream as a value.
                punct.push('\'');
            }
            '-' if next == Some('-') => {
                flush(&mut word, &mut words);
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                flush(&mut word, &mut words);
                i += 2;
                while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                    i += 1;
                }
                i += 1;
            }
            _ if ch.is_alphanumeric() || "_@#$".contains(ch) => word.push(ch),
            _ => {
                flush(&mut word, &mut words);
                if !ch.is_whitespace() {
                    punct.push(ch);
                    if ch == ';' {
                        words.push(";".into());
                    }
                }
            }
        }
        i += 1;
    }
    flush(&mut word, &mut words);
    (words, punct)
}

/// One statement that only reads: it starts with SELECT or WITH, has nothing after a `;`,
/// and none of the words that write, change the schema or reach outside the database
/// (outside strings, quoted names and comments). The database account's own rights are
/// the real boundary; this keeps a mistake in the settings from ever writing.
pub fn read_only(sql: &str) -> Result<(), String> {
    // MySQL's dialect reads more as code than this does: `/*! … */` runs, `#` starts a
    // comment, `\'` doesn't end a string; with them a write could hide in what looks
    // like a comment or a string here (bug round F, F3-14). Refused.
    if sql.contains("/*!") || sql.contains('\\') {
        return Err("`/*!` and backslashes aren't allowed in a read-only query".into());
    }
    let (words, _) = tokens(sql);
    if words.iter().any(|w| w.contains('#')) {
        return Err("`#` (a comment in MySQL) isn't allowed in a read-only query".into());
    }
    match words.first().map(String::as_str) {
        Some("SELECT" | "WITH") => {}
        _ => return Err("a query must start with SELECT or WITH".into()),
    }
    if let Some(i) = words.iter().position(|w| w == ";")
        && i + 1 < words.len()
    {
        return Err("one statement only (something follows a `;`)".into());
    }
    for w in &words {
        if WRITES.contains(&w.as_str()) || w.starts_with("XP_") || w.starts_with("SP_") {
            return Err(format!("`{w}` isn't allowed in a read-only query"));
        }
    }
    Ok(())
}

/// The number of `?` placeholders outside strings and comments.
pub(super) fn placeholders(sql: &str) -> usize {
    tokens(sql).1.iter().filter(|&&c| c == '?').count()
}
