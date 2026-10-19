//! Port of `src/aimap/profiles.py`: profiles (recipient context) and their
//! patterns, stored in Postgres.
//!
//! A profile is free-form JSON that Jev reads as `recipient_profile`. Every
//! account uses one profile, `default` unless changed with
//! `aimap accounts set-profile`.

use postgres::GenericClient;
use serde_json::Value;

use crate::jev::{JevError, Pattern};

#[derive(Debug)]
pub enum ProfileError {
    /// `ProfileError` in profile.py
    Invalid(String),
    Postgres(postgres::Error),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::Invalid(m) => write!(f, "{m}"),
            ProfileError::Postgres(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ProfileError {}

impl From<postgres::Error> for ProfileError {
    fn from(e: postgres::Error) -> Self {
        ProfileError::Postgres(e)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub profile: Value,
}

pub fn get(conn: &mut impl GenericClient, name: &str) -> Result<Profile, ProfileError> {
    let row = conn
        .query_opt("SELECT id, name, profile FROM profiles WHERE name = $1", &[&name])?
        .ok_or_else(|| ProfileError::Invalid(format!("no profile '{name}'")))?;
    Ok(Profile { id: row.get(0), name: row.get(1), profile: row.get(2) })
}

/// (name, accounts, patterns) per profile.
pub fn list_all(conn: &mut impl GenericClient) -> Result<Vec<(String, i64, i64)>, postgres::Error> {
    Ok(conn
        .query(
            "SELECT p.name,
                    (SELECT count(*) FROM accounts a WHERE a.profile_id = p.id),
                    (SELECT count(*) FROM patterns t WHERE t.profile_id = p.id)
             FROM profiles p ORDER BY p.name",
            &[],
        )?
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect())
}

pub fn put(conn: &mut impl GenericClient, name: &str, profile: &Value) -> Result<(), ProfileError> {
    if !profile.is_object() {
        return Err(ProfileError::Invalid("a profile must be a JSON object".to_string()));
    }
    conn.execute(
        "INSERT INTO profiles (name, profile) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET profile = EXCLUDED.profile, updated_at = now()",
        &[&name, &profile],
    )?;
    Ok(())
}

pub fn assign(conn: &mut impl GenericClient, address: &str, name: &str) -> Result<(), ProfileError> {
    let profile = get(conn, name)?;
    conn.execute(
        "INSERT INTO accounts (address, profile_id) VALUES ($1, $2)
         ON CONFLICT (address) DO UPDATE SET profile_id = EXCLUDED.profile_id",
        &[&address, &profile.id],
    )?;
    Ok(())
}

pub fn account_profiles(conn: &mut impl GenericClient) -> Result<Vec<(String, String)>, postgres::Error> {
    Ok(conn
        .query(
            "SELECT a.address, p.name FROM accounts a JOIN profiles p ON p.id = a.profile_id
             ORDER BY a.address",
            &[],
        )?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect())
}

pub fn patterns(conn: &mut impl GenericClient, profile_id: i64) -> Result<Vec<Pattern>, postgres::Error> {
    Ok(conn
        .query(
            "SELECT insight, importance, action_bucket, tags, examples FROM patterns
             WHERE profile_id = $1 ORDER BY insight",
            &[&profile_id],
        )?
        .iter()
        .map(|r| {
            let examples: Value = r.get(4);
            Pattern {
                insight: r.get(0),
                importance: r.get(1),
                action_bucket: r.get(2),
                tags: r.get(3),
                examples: match examples {
                    Value::Array(items) => items,
                    Value::Null => Vec::new(),
                    other => vec![other],
                },
            }
        })
        .collect())
}

/// Replace every pattern of the profile. Changes the classifier key, so new
/// mail uses the new set.
pub fn replace_patterns(
    conn: &mut impl GenericClient,
    profile_id: i64,
    items: &[Pattern],
) -> Result<(), ProfileError> {
    let mut seen = std::collections::HashSet::new();
    for p in items {
        p.validate().map_err(|JevError(m)| ProfileError::Invalid(m))?;
        if !seen.insert(p.insight.clone()) {
            return Err(ProfileError::Invalid(format!("duplicate pattern '{}'", p.insight)));
        }
    }
    conn.execute("DELETE FROM patterns WHERE profile_id = $1", &[&profile_id])?;
    for p in items {
        conn.execute(
            "INSERT INTO patterns (profile_id, insight, importance, action_bucket, tags, examples)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &profile_id,
                &p.insight,
                &p.importance,
                &p.action_bucket,
                &p.tags,
                &Value::Array(p.examples.clone()),
            ],
        )?;
    }
    Ok(())
}

/// JSON: a list of {insight, importance, action_bucket, tags, examples}.
/// CSV: columns insight, importance, action_bucket, tags (';'-separated), and
/// optional example_from / example_subject (rows with the same insight add
/// examples).
pub fn parse_patterns(text: &str, fmt: &str) -> Result<Vec<Pattern>, ProfileError> {
    match fmt {
        "json" => {
            let data: Value = serde_json::from_str(text)
                .map_err(|e| ProfileError::Invalid(format!("patterns JSON: {e}")))?;
            let Value::Array(items) = data else {
                return Err(ProfileError::Invalid("patterns JSON must be a list".to_string()));
            };
            let mut out = Vec::new();
            for d in &items {
                let obj = d.as_object().ok_or_else(|| {
                    ProfileError::Invalid("invalid pattern: item is not an object".to_string())
                })?;
                let string_list = |key: &str| -> Result<Vec<String>, ProfileError> {
                    match obj.get(key) {
                        None | Some(Value::Null) => Ok(Vec::new()),
                        Some(Value::Array(items)) => Ok(items
                            .iter()
                            .map(|v| v.as_str().unwrap_or_default().to_string())
                            .collect()),
                        Some(other) => Err(ProfileError::Invalid(format!(
                            "invalid pattern: {key} must be a list, got {other}"
                        ))),
                    }
                };
                let value_list = |key: &str| -> Result<Vec<Value>, ProfileError> {
                    match obj.get(key) {
                        None | Some(Value::Null) => Ok(Vec::new()),
                        Some(Value::Array(items)) => Ok(items.clone()),
                        Some(other) => Err(ProfileError::Invalid(format!(
                            "invalid pattern: {key} must be a list, got {other}"
                        ))),
                    }
                };
                let field = |key: &str| -> Result<String, ProfileError> {
                    obj.get(key).map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    }).ok_or_else(|| {
                        ProfileError::Invalid(format!("invalid pattern: '{key}'"))
                    })
                };
                out.push(Pattern {
                    insight: field("insight")?,
                    importance: field("importance")?,
                    action_bucket: field("action_bucket")?,
                    tags: string_list("tags")?,
                    examples: value_list("examples")?,
                });
            }
            Ok(out)
        }
        "csv" => {
            let rows = csv_rows(text).map_err(ProfileError::Invalid)?;
            let mut rows = rows.iter();
            let header = rows.next().ok_or_else(|| {
                ProfileError::Invalid("CSV needs insight, importance, action_bucket columns (empty file)".to_string())
            })?;
            let column = |name: &str| header.iter().position(|h| h == name);
            let insight_col = column("insight");
            let importance_col = column("importance");
            let action_col = column("action_bucket");
            let tags_col = column("tags");
            let ex_from_col = column("example_from");
            let ex_subject_col = column("example_subject");

            let mut merged: Vec<(String, Pattern)> = Vec::new();
            let mut by_insight: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for row in rows {
                let cell = |i: Option<usize>| -> Option<String> {
                    i.and_then(|i| row.get(i)).filter(|c| !c.is_empty()).cloned()
                };
                let (Some(insight_col), Some(importance_col), Some(action_col)) = (insight_col, importance_col, action_col) else {
                    return Err(ProfileError::Invalid(
                        "CSV needs insight, importance, action_bucket columns ('insight')".to_string(),
                    ));
                };
                // a row shorter than the header leaves required fields unset
                // (None), which Python trips over exactly the same way
                let Some(insight_raw) = row.get(insight_col) else {
                    return Err(ProfileError::Invalid(
                        "CSV needs insight, importance, action_bucket columns ('insight')".to_string(),
                    ));
                };
                if row.get(importance_col).is_none() || row.get(action_col).is_none() {
                    return Err(ProfileError::Invalid(
                        "CSV needs insight, importance, action_bucket columns ('importance')".to_string(),
                    ));
                }
                let insight = insight_raw.trim().to_string();
                let idx = match by_insight.get(&insight) {
                    Some(&i) => i,
                    None => {
                        let p = Pattern {
                            insight: insight.clone(),
                            importance: row
                                .get(importance_col)
                                .filter(|c| !c.is_empty())
                                .map(|c| c.trim().to_string())
                                .unwrap_or_default(),
                            action_bucket: row
                                .get(action_col)
                                .filter(|c| !c.is_empty())
                                .map(|c| c.trim().to_string())
                                .unwrap_or_default(),
                            tags: cell(tags_col)
                                .map(|t| {
                                    t.split(';')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            examples: Vec::new(),
                        };
                        merged.push((insight.clone(), p));
                        by_insight.insert(insight.clone(), merged.len() - 1);
                        merged.len() - 1
                    }
                };
                // `if row.get("example_subject") and len(examples) < 3`
                let subject = cell(ex_subject_col);
                if let Some(subject) = subject {
                    let p = &mut merged[idx].1;
                    if p.examples.len() < 3 {
                        p.examples.push(serde_json::json!({
                            "from": cell(ex_from_col).unwrap_or_default(),
                            "subject": subject.trim().to_string(),
                        }));
                    }
                }
            }
            Ok(merged.into_iter().map(|(_, p)| p).collect())
        }
        other => Err(ProfileError::Invalid(format!("unknown patterns format '{other}'"))),
    }
}

/// A minimal RFC 4180 reader matching Python's `csv.reader` defaults:
/// comma delimiter, `"` quotes, doubled quotes inside quoted fields, records
/// ended by \r\n, \n or \r, and blank lines producing (skipped) empty rows.
/// Unlike the csv module, embedded newlines inside quotes are preserved as \n.
fn csv_rows(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                '\n' => field.push('\n'),
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            ',' => {
                row.push(std::mem::take(&mut field));
            }
            '\n' | '\r' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                // a blank line is an empty record, like csv.reader
                if row.is_empty() && field.is_empty() {
                    rows.push(Vec::new());
                } else {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_reader_matches_python_reader_defaults() {
        // plain rows
        assert_eq!(
            csv_rows("a,b,c\n1,2,3\n").unwrap(),
            vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]
        );
        // quoted fields with commas and doubled quotes
        assert_eq!(
            csv_rows("\"has,comma\",\"say \"\"hi\"\"\",plain").unwrap(),
            vec![vec!["has,comma", "say \"hi\"", "plain"]]
        );
        // CRLF and lone CR record separators
        assert_eq!(csv_rows("a,b\r\n1,2\r\n").unwrap(), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(csv_rows("a,b\r1,2").unwrap(), vec![vec!["a", "b"], vec!["1", "2"]]);
        // blank lines yield empty rows (DictReader skips them)
        assert_eq!(csv_rows("a,b\n\n1,2\n").unwrap(), vec![vec!["a", "b"], vec![], vec!["1", "2"]]);
        // no trailing newline
        assert_eq!(csv_rows("a,b").unwrap(), vec![vec!["a", "b"]]);
        // quoted newline stays inside the field
        assert_eq!(csv_rows("\"line1\nline2\",x").unwrap(), vec![vec!["line1\nline2", "x"]]);
    }
}
