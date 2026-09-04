//! Schema fingerprinting: compare the database SQLite actually has with the
//! declared whole schema in `migrations/0001_init.sql`.
//!
//! SQLite stores `CREATE` statements as written, comments and all, so two
//! databases built from the same logical schema but different comments or
//! whitespace would otherwise look different. The normaliser here strips the
//! cosmetic differences that do not change what the schema means.

use super::{Result, StoreError};
use turso::Connection;

/// The declared migrations, in order. An empty database is built by applying
/// them one after another; each one applies cleanly to what the ones before
/// it built, so their concatenation is itself a valid whole-schema script.
pub(crate) const MIGRATIONS: &[&str] = &[include_str!("../../migrations/0001_init.sql")];

/// The whole declared schema — every migration applied in order — as one
/// batch of SQL.
pub(crate) fn schema_sql() -> String {
    let mut sql = String::new();
    for migration in MIGRATIONS {
        sql.push_str(migration);
        sql.push('\n');
    }
    sql
}

/// Reads and normalises the schema of the connected database.
///
/// The fingerprint is a deterministic text built from `sqlite_master`:
/// `type|name|normalised_sql` for every non-sqlite object, ordered by type
/// and name. Two schemas that mean the same thing produce the same string;
/// two schemas that differ in tables, indexes, columns or constraints do not.
pub async fn fingerprint(conn: &Connection) -> Result<String> {
    let mut rows = conn
        .query(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
            (),
        )
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

    let mut out = String::new();
    while let Some(row) = rows.next().await.map_err(|e| StoreError::Backend(e.to_string()))? {
        let t: String = row.get(0).map_err(|e| StoreError::Backend(e.to_string()))?;
        let name: String = row.get(1).map_err(|e| StoreError::Backend(e.to_string()))?;
        let sql: Option<String> =
            row.get(2).map_err(|e| StoreError::Backend(e.to_string()))?;
        out.push_str(&t);
        out.push('|');
        out.push_str(&name);
        out.push('|');
        if let Some(sql) = sql {
            out.push_str(&normalize_schema(&sql));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Builds an in-memory database from the declared schema and fingerprints it.
pub async fn declared_fingerprint() -> Result<String> {
    let db = turso::Builder::new_local(":memory:")
        .build()
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let conn = db.connect().map_err(|e| StoreError::Backend(e.to_string()))?;
    conn.execute_batch(&schema_sql())
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    fingerprint(&conn).await
}

/// A single schema object for diff reporting.
#[derive(Debug, Clone)]
pub(crate) struct SchemaObject {
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) sql: String,
}

/// Parses the normalised schema into objects keyed by (kind, name).
pub(crate) fn parse_objects(fingerprint: &str) -> Vec<SchemaObject> {
    fingerprint
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let kind = parts.next()?.to_string();
            let name = parts.next()?.to_string();
            let sql = parts.next().unwrap_or("").to_string();
            Some(SchemaObject { kind, name, sql })
        })
        .collect()
}

/// Human-readable description of how two fingerprints differ.
pub(crate) fn diff_report(old: &str, new: &str) -> String {
    let old_objs = parse_objects(old);
    let new_objs = parse_objects(new);

    let mut missing = Vec::new();
    let mut extra = Vec::new();
    let mut changed = Vec::new();

    for old_obj in &old_objs {
        match new_objs.iter().find(|n| n.kind == old_obj.kind && n.name == old_obj.name) {
            Some(new_obj) if new_obj.sql != old_obj.sql => changed.push((old_obj, new_obj)),
            Some(_) => {}
            None => missing.push(old_obj),
        }
    }
    for new_obj in &new_objs {
        if !old_objs.iter().any(|o| o.kind == new_obj.kind && o.name == new_obj.name) {
            extra.push(new_obj);
        }
    }

    let mut lines = Vec::new();
    for obj in missing {
        lines.push(format!("- {} {} (removed)", obj.kind, obj.name));
    }
    for obj in extra {
        lines.push(format!("+ {} {} (added)", obj.kind, obj.name));
    }
    for (old_obj, new_obj) in changed {
        lines.push(format!("~ {} {} (changed)", old_obj.kind, old_obj.name));
        if old_obj.kind == "table" {
            let old_cols = extract_column_names(&old_obj.sql);
            let new_cols = extract_column_names(&new_obj.sql);
            let added: Vec<&str> = new_cols
                .iter()
                .filter(|c| !old_cols.contains(c))
                .map(String::as_str)
                .collect();
            let removed: Vec<&str> = old_cols
                .iter()
                .filter(|c| !new_cols.contains(c))
                .map(String::as_str)
                .collect();
            if !added.is_empty() {
                lines.push(format!("    added columns: {}", added.join(", ")));
            }
            if !removed.is_empty() {
                lines.push(format!("    removed columns: {}", removed.join(", ")));
            }
        }
    }

    if lines.is_empty() {
        "schemas differ (cosmetic normalisation only)".to_string()
    } else {
        lines.join("\n")
    }
}

/// Extracts the column names from a normalised CREATE TABLE statement.
/// Constraints at the table level are ignored; only top-level definitions
/// that start with an identifier are collected.
fn extract_column_names(sql: &str) -> Vec<String> {
    let body = match sql.strip_prefix("CREATE TABLE ") {
        Some(rest) => rest,
        None => return Vec::new(),
    };
    let body = remove_if_not_exists(body);
    let Some(start) = body.find('(') else {
        return Vec::new();
    };
    let Some(end) = body.rfind(')') else {
        return Vec::new();
    };
    let body = &body[start + 1..end];

    let mut cols = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                if let Some(col) = first_identifier(&current) {
                    cols.push(col);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        if let Some(col) = first_identifier(&current) {
            cols.push(col);
        }
    }
    cols
}

fn first_identifier(item: &str) -> Option<String> {
    let item = item.trim();
    if item.is_empty() {
        return None;
    }
    let first = item.split_whitespace().next()?;
    let first = first.to_lowercase();
    if first == "constraint"
        || first == "primary"
        || first == "unique"
        || first == "check"
        || first == "foreign"
    {
        return None;
    }
    Some(first)
}

/// Normalises SQL text so that cosmetic differences disappear.
///
/// Differences deliberately ignored:
/// - whitespace runs collapse to a single space;
/// - SQL comments (`--` to end of line, `/* ... */`) are removed;
/// - `IF NOT EXISTS` is removed (case-insensitive, whole tokens only).
///
/// Differences deliberately preserved:
/// - identifier case and spelling;
/// - string literal contents, including any whitespace or comment-like text
///   inside them;
/// - numeric literals, keyword order and constraint text.
fn normalize_schema(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    let mut prev_ws = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    out.push(chars.next().unwrap());
                } else {
                    in_string = false;
                }
            }
            continue;
        }

        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\n' {
                    break;
                }
            }
            prev_ws = true;
            continue;
        }

        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            prev_ws = true;
            continue;
        }

        if ch == '\'' {
            in_string = true;
            out.push(ch);
            prev_ws = false;
            continue;
        }

        if ch.is_whitespace() {
            prev_ws = true;
            continue;
        }

        if prev_ws && !out.is_empty() {
            out.push(' ');
        }
        out.push(ch);
        prev_ws = false;
    }

    remove_if_not_exists(&out)
}

/// Strips a normalised `IF NOT EXISTS` token sequence, preserving whitespace
/// around it. Because `normalize_schema` has already collapsed whitespace, the
/// sequence appears as separate tokens separated by single spaces.
fn remove_if_not_exists(sql: &str) -> String {
    let tokens: Vec<&str> = sql.split(' ').collect();
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if i + 2 < tokens.len()
            && tokens[i].eq_ignore_ascii_case("IF")
            && tokens[i + 1].eq_ignore_ascii_case("NOT")
            && tokens[i + 2].eq_ignore_ascii_case("EXISTS")
        {
            i += 3;
        } else {
            out.push(tokens[i]);
            i += 1;
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::{normalize_schema, parse_objects};

    #[test]
    fn comments_and_whitespace_do_not_change_meaning() {
        let a = "CREATE TABLE foo (\n    id INTEGER, -- the key\n    name TEXT /* a comment */\n);";
        let b = "CREATE TABLE foo ( id INTEGER, name TEXT );";
        assert_eq!(normalize_schema(a), normalize_schema(b));
    }

    #[test]
    fn if_not_exists_is_ignored() {
        let a = "CREATE TABLE IF NOT EXISTS foo (id INTEGER);";
        let b = "CREATE TABLE foo (id INTEGER);";
        assert_eq!(normalize_schema(a), normalize_schema(b));
    }

    #[test]
    fn string_literals_are_preserved() {
        let a = "CREATE TABLE foo (name TEXT DEFAULT 'a -- b /* c */');";
        let b = "CREATE TABLE foo (name TEXT DEFAULT 'a -- b /* c */');";
        assert_eq!(normalize_schema(a), normalize_schema(b));
    }

    #[test]
    fn real_differences_still_differ() {
        let a = "CREATE TABLE foo (id INTEGER);";
        let b = "CREATE TABLE foo (id TEXT);";
        assert_ne!(normalize_schema(a), normalize_schema(b));
    }

    #[test]
    fn if_not_exists_does_not_corrupt_identifiers() {
        // The stripper looks for whole tokens; words that merely appear
        // inside identifiers or string literals must stay put.
        let a = "CREATE TABLE IF NOT EXISTS notification (if_not_exists TEXT DEFAULT 'IF NOT EXISTS');";
        let b = "CREATE TABLE notification (if_not_exists TEXT DEFAULT 'IF NOT EXISTS');";
        assert_eq!(normalize_schema(a), normalize_schema(b));
    }

    #[test]
    fn partial_index_where_clause_is_parsed_safely() {
        // Partial-index SQL contains a WHERE clause; diff_report must not
        // try to extract column names from it, and parsing must not panic.
        let fp = "index|tag_one_default|CREATE UNIQUE INDEX tag_one_default ON tag(board_id) WHERE is_default = 1\n";
        let objs = parse_objects(fp);
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].kind, "index");
        assert_eq!(objs[0].name, "tag_one_default");
    }
}
