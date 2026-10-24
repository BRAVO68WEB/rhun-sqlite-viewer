//! Split a SQL script into statements and quote identifiers and literals.

/// Double-quote a SQLite identifier. Embedded quotes are doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `INSERT INTO t (cols) VALUES (?, ?, ?)`, or `DEFAULT VALUES` when there are no columns.
pub fn insert_statement(table: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        return format!("INSERT INTO {} DEFAULT VALUES", quote_ident(table));
    }
    let names = columns
        .iter()
        .map(|name| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ");
    let marks = vec!["?"; columns.len()].join(", ");
    format!(
        "INSERT INTO {} ({names}) VALUES ({marks})",
        quote_ident(table)
    )
}

/// `UPDATE t SET col = ? WHERE ...`
pub fn update_statement(table: &str, column: &str, predicate: &str) -> String {
    format!(
        "UPDATE {} SET {} = ? WHERE {predicate}",
        quote_ident(table),
        quote_ident(column)
    )
}

/// A SQL literal: NULL, or a single-quoted string.
pub fn quote_lit(value: &Option<String>) -> String {
    match value {
        None => "NULL".into(),
        Some(text) => format!("'{}'", text.replace('\'', "''")),
    }
}

/// Statements of `input`, split on semicolons that are not inside quotes, comments, or a
/// trigger body. Comment-only pieces are left out.
pub fn split_sql(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut mode = Mode::Code;
    let mut word = String::new();
    let mut saw_create = false;
    let mut trigger = false;
    let mut depth = 0i32;
    let mut case_depth = 0i32;
    let mut significant = false;
    let chars: Vec<(usize, char)> = input.char_indices().collect();

    // CREATE [TEMP] TRIGGER … BEGIN … END is one statement. END closes a CASE first, so a
    // CASE expression inside the trigger body does not finish the trigger.
    let flush_word = |word: &mut String,
                      saw_create: &mut bool,
                      trigger: &mut bool,
                      depth: &mut i32,
                      case_depth: &mut i32| {
        if word.is_empty() {
            return;
        }
        let upper = word.to_ascii_uppercase();
        if !*trigger {
            if upper == "CREATE" || (*saw_create && (upper == "TEMP" || upper == "TEMPORARY")) {
                *saw_create = true;
            } else if *saw_create && upper == "TRIGGER" {
                *trigger = true;
                *saw_create = false;
            } else {
                *saw_create = false;
            }
        }
        if upper == "CASE" {
            *case_depth += 1;
        } else if upper == "END" && *case_depth > 0 {
            *case_depth -= 1;
        } else if *trigger && upper == "BEGIN" {
            *depth += 1;
        } else if *trigger && upper == "END" && *depth > 0 {
            *depth -= 1;
        }
        word.clear();
    };

    let mut i = 0;
    while i < chars.len() {
        let (at, ch) = chars[i];
        let next = chars.get(i + 1).map(|c| c.1);
        match mode {
            Mode::Code => {
                if ch == '-' && next == Some('-') {
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    mode = Mode::Line;
                    i += 2;
                    continue;
                }
                if ch == '/' && next == Some('*') {
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    mode = Mode::Block;
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    significant = true;
                    mode = Mode::Single;
                    i += 1;
                    continue;
                }
                if ch == '"' || ch == '`' || ch == '[' {
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    significant = true;
                    mode = match ch {
                        '"' => Mode::Double,
                        '`' => Mode::Backtick,
                        _ => Mode::Bracket,
                    };
                    i += 1;
                    continue;
                }
                if ch == ';' {
                    // Flush first so END is counted before this semicolon decides to split.
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    if !trigger || depth == 0 {
                        if significant {
                            let stmt = input[start..at].trim();
                            if !stmt.is_empty() {
                                out.push(stmt.to_string());
                            }
                        }
                        trigger = false;
                        depth = 0;
                        case_depth = 0;
                        saw_create = false;
                        significant = false;
                        start = chars.get(i + 1).map(|c| c.0).unwrap_or(input.len());
                    }
                    i += 1;
                    continue;
                }
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    word.push(ch);
                    significant = true;
                } else {
                    flush_word(
                        &mut word,
                        &mut saw_create,
                        &mut trigger,
                        &mut depth,
                        &mut case_depth,
                    );
                    if !ch.is_whitespace() {
                        significant = true;
                    }
                }
                i += 1;
            }
            Mode::Line => {
                if ch == '\n' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::Block => {
                if ch == '*' && next == Some('/') {
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Mode::Single => {
                if ch == '\'' && next == Some('\'') {
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::Double => {
                if ch == '"' && next == Some('"') {
                    i += 2;
                    continue;
                }
                if ch == '"' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::Backtick => {
                if ch == '`' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::Bracket => {
                if ch == ']' {
                    mode = Mode::Code;
                }
                i += 1;
            }
        }
    }
    flush_word(
        &mut word,
        &mut saw_create,
        &mut trigger,
        &mut depth,
        &mut case_depth,
    );
    if significant {
        let stmt = input[start..].trim();
        if !stmt.is_empty() {
            out.push(stmt.to_string());
        }
    }
    out
}

enum Mode {
    Code,
    Line,
    Block,
    Single,
    Double,
    Backtick,
    Bracket,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_idents_and_literals() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_lit(&None), "NULL");
        assert_eq!(quote_lit(&Some("a'b".into())), "'a''b'");
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_and_comments() {
        let parts = split_sql("select 1; select 'a;b'; select 2 -- ;\n; select \"c;d\"");
        assert_eq!(
            parts,
            vec![
                "select 1",
                "select 'a;b'",
                "select 2 -- ;",
                "select \"c;d\""
            ]
        );
    }

    #[test]
    fn keeps_a_trigger_body_together() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON notes BEGIN\n  \
                   INSERT INTO log(body) VALUES ('a;b');\nEND;\nSELECT 1;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("INSERT INTO log"));
        assert_eq!(parts[1], "SELECT 1");
    }

    #[test]
    fn skips_comments() {
        assert!(split_sql("-- only\n/* ; */").is_empty());
    }

    #[test]
    fn keeps_case_end_inside_a_temp_trigger() {
        let sql = "CREATE TEMP TRIGGER t AFTER INSERT ON notes BEGIN\n  \
                   INSERT INTO log(body) SELECT CASE WHEN 1 THEN 'a;b' END;\nEND;\nSELECT 1;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("CASE"));
        assert_eq!(parts[1], "SELECT 1");
    }
}
