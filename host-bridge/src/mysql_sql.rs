//! MySQL dialect translation for the SQL frame.data generates.
//!
//! frame.data emits one SQL dialect — the one PostgreSQL and SQLite share:
//! numbered `$n` placeholders, `INSERT ... RETURNING`, `INSERT ... DEFAULT
//! VALUES`, `CREATE INDEX IF NOT EXISTS`, `SERIAL` keys, inline
//! `REFERENCES`, `INTERVAL '<n> <unit>'`. MySQL 8 accepts none of these, so
//! the MySQL driver passes every statement through [`translate`] right
//! before it runs. The result is a list of [`Statement`]s (the input may
//! hold several, separated by `;`), each with its `?`-placeholder SQL, the
//! parameter index every `?` binds, and an [`Action`] for the shapes that
//! need the database's help (the row an `INSERT ... RETURNING` created, the
//! table a bare `DROP INDEX` names, which index columns are `TEXT`).
//!
//! Only shapes that MySQL rejects are rewritten, so SQL written for MySQL
//! (`?` placeholders, backtick identifiers, `ON DUPLICATE KEY UPDATE`, ...)
//! runs unchanged. Shapes whose meaning differs between the dialects and
//! cannot be told apart from valid MySQL — `"..."` (an identifier in
//! PostgreSQL, a string in MySQL) and backslashes inside `'...'` — are left
//! as MySQL reads them. `||` needs no rewrite: the driver's connections run
//! with `PIPES_AS_CONCAT`, so it concatenates as in PostgreSQL.

use std::collections::HashSet;

/// One statement ready to run on MySQL.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Statement {
    /// The statement text with `?` placeholders. For [`Action::CreateIndex`]
    /// and [`Action::DropIndex`] the text is produced by the action instead.
    pub sql: String,
    /// For every `?` in `sql`, in order, the index of the caller's parameter
    /// it binds. A `$n` used twice binds the same parameter twice.
    pub binds: Vec<usize>,
    pub action: Action,
}

/// What running a [`Statement`] involves beyond executing `sql`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    /// Execute `sql` as it is.
    Run,
    /// `INSERT ... RETURNING <columns>`: execute the INSERT (`sql`, without
    /// the RETURNING clause), then answer the requested columns of the new
    /// rows, found by the table's AUTO_INCREMENT key.
    InsertReturning { table: TableRef, columns: Returning },
    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] ...`: skipped when it already
    /// exists (IF NOT EXISTS); `TEXT`/`BLOB` columns get a key prefix.
    CreateIndex(IndexSpec),
    /// `DROP INDEX [IF EXISTS] <name> [ON <table>]`: MySQL needs the table,
    /// which is looked up when the statement does not name it.
    DropIndex {
        name: String,
        table: Option<TableRef>,
        if_exists: bool,
    },
}

/// A table named in a statement: `table` or `schema.table`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    /// The table as written in the statement (quotes kept).
    pub sql: String,
}

/// The columns of a `RETURNING` clause.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Returning {
    /// `RETURNING *`
    All,
    /// `RETURNING a, b AS c`
    Columns(Vec<ReturnColumn>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReturnColumn {
    /// The column name (unquoted).
    pub column: String,
    /// The name the result uses: the alias, else the column.
    pub label: String,
}

/// A parsed `CREATE [UNIQUE] INDEX` statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexSpec {
    pub unique: bool,
    pub if_not_exists: bool,
    pub name: String,
    pub table: TableRef,
    pub columns: Vec<IndexColumn>,
    /// Anything after the column list (`USING BTREE`, ...).
    pub tail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexColumn {
    /// The column name when the entry is a plain column (optionally followed
    /// by ASC/DESC) without a prefix length; `None` for expressions.
    pub column: Option<String>,
    /// The entry as written, without the ASC/DESC suffix.
    pub sql: String,
    /// ` ASC` / ` DESC`, or empty.
    pub suffix: String,
}

/// The key prefix (in characters) a `TEXT`/`BLOB` index column gets: MySQL
/// only indexes those types by prefix, and 255 characters keep a three-column
/// utf8mb4 index inside InnoDB's 3072-byte key limit.
pub(crate) const TEXT_KEY_PREFIX: usize = 255;

impl IndexSpec {
    /// The `CREATE INDEX` statement for MySQL. `text_columns` holds the
    /// (lower-cased) names of the table's `TEXT`/`BLOB` columns.
    pub fn render(&self, text_columns: &HashSet<String>) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| match &c.column {
                Some(name) if text_columns.contains(&name.to_ascii_lowercase()) => {
                    format!("{}({}){}", c.sql, TEXT_KEY_PREFIX, c.suffix)
                }
                _ => format!("{}{}", c.sql, c.suffix),
            })
            .collect();
        let mut sql = format!(
            "CREATE {}INDEX {} ON {} ({})",
            if self.unique { "UNIQUE " } else { "" },
            self.name,
            self.table.sql,
            cols.join(", ")
        );
        if !self.tail.is_empty() {
            sql.push(' ');
            sql.push_str(&self.tail);
        }
        sql
    }
}

/// Translate `sql` (which binds `param_count` parameters) for MySQL.
///
/// Errors name the shape MySQL cannot run: mixing `?` with `$n`, a `$n`
/// without a value, `RETURNING` on UPDATE/DELETE, a data-modifying CTE.
pub(crate) fn translate(sql: &str, param_count: usize) -> Result<Vec<Statement>, String> {
    let tokens = lex(sql);
    let numbered = tokens
        .iter()
        .any(|t| matches!(t.kind, Kind::Param(_)) && t.text.starts_with('$'));
    let positional = tokens.iter().any(|t| t.kind == Kind::QMark);
    if numbered && positional {
        return Err("SQL mixes `?` and `$n` placeholders; use one style per statement".to_string());
    }

    // `?` placeholders become `$k` (k counting across statements), so the
    // rewrites below can move or repeat a placeholder with its value.
    let mut next = 0usize;
    let tokens: Vec<Tok> = tokens
        .into_iter()
        .map(|t| {
            if t.kind == Kind::QMark {
                next += 1;
                Tok {
                    kind: Kind::Param(next),
                    text: format!("${}", next),
                }
            } else {
                t
            }
        })
        .collect();

    let mut statements = Vec::new();
    for part in split_statements(tokens) {
        statements.push(translate_statement(part, param_count, positional)?);
    }

    if positional {
        if statements.len() == 1 {
            // A single `?` statement binds the caller's parameters as given,
            // so MySQL still reports a count mismatch itself.
            statements[0].binds = (0..param_count).collect();
        } else if let Some(n) = statements
            .iter()
            .flat_map(|s| s.binds.iter())
            .find(|&&i| i >= param_count)
        {
            return Err(format!(
                "placeholder `?` number {} has no value ({} parameter{} given)",
                n + 1,
                param_count,
                if param_count == 1 { "" } else { "s" }
            ));
        }
    }
    Ok(statements)
}

// ============================================================================
// Lexer
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// Identifier, keyword or number.
    Word,
    /// `'...'`, `"..."` or `` `...` ``.
    Quoted,
    /// `$n` (or a `?` once numbered by [`translate`]).
    Param(usize),
    QMark,
    Punct,
    Space,
    Comment,
}

#[derive(Debug, Clone, PartialEq)]
struct Tok {
    kind: Kind,
    text: String,
}

impl Tok {
    fn word(text: &str) -> Tok {
        Tok {
            kind: Kind::Word,
            text: text.to_string(),
        }
    }

    fn punct(c: char) -> Tok {
        Tok {
            kind: Kind::Punct,
            text: c.to_string(),
        }
    }

    fn space() -> Tok {
        Tok {
            kind: Kind::Space,
            text: " ".to_string(),
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(kw)
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == Kind::Punct && self.text.len() == 1 && self.text.starts_with(c)
    }

    fn significant(&self) -> bool {
        !matches!(self.kind, Kind::Space | Kind::Comment)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn lex(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let take = |from: usize, to: usize| -> String { chars[from..to].iter().collect() };
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_whitespace() {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok {
                kind: Kind::Space,
                text: take(start, i),
            });
        } else if (c == '-'
            && chars.get(i + 1) == Some(&'-')
            && chars.get(i + 2).is_none_or(|n| n.is_whitespace()))
            || c == '#'
        {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(Tok {
                kind: Kind::Comment,
                text: take(start, i),
            });
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            out.push(Tok {
                kind: Kind::Comment,
                text: take(start, i),
            });
        } else if c == '\'' || c == '"' || c == '`' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && c != '`' {
                    i += 2;
                    continue;
                }
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(chars.len());
            out.push(Tok {
                kind: Kind::Quoted,
                text: take(start, end),
            });
            i = end;
        } else if c == '$' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let digits = take(start + 1, i);
            out.push(Tok {
                kind: Kind::Param(digits.parse().unwrap_or(0)),
                text: take(start, i),
            });
        } else if c == '?' {
            i += 1;
            out.push(Tok {
                kind: Kind::QMark,
                text: "?".to_string(),
            });
        } else if is_word_char(c) {
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            out.push(Tok {
                kind: Kind::Word,
                text: take(start, i),
            });
        } else {
            i += 1;
            out.push(Tok::punct(c));
        }
    }
    out
}

fn render(tokens: &[Tok]) -> String {
    tokens.iter().map(|t| t.text.as_str()).collect()
}

/// Split at top-level `;`, dropping empty statements.
fn split_statements(tokens: Vec<Tok>) -> Vec<Vec<Tok>> {
    let mut parts = Vec::new();
    let mut current = Vec::new();
    for t in tokens {
        if t.is_punct(';') {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(t);
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(trim)
        .filter(|p| p.iter().any(Tok::significant))
        .collect()
}

/// Drop leading and trailing whitespace.
fn trim(mut tokens: Vec<Tok>) -> Vec<Tok> {
    while tokens.last().is_some_and(|t| t.kind == Kind::Space) {
        tokens.pop();
    }
    let lead = tokens.iter().take_while(|t| t.kind == Kind::Space).count();
    tokens.drain(..lead);
    tokens
}

/// The position in `tokens` of its `k`-th significant token (`tokens.len()`
/// past the last).
fn position_of(tokens: &[Tok], k: usize) -> usize {
    tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.significant())
        .nth(k)
        .map(|(i, _)| i)
        .unwrap_or(tokens.len())
}

/// Keywords that keep a space before a following `(`; any other word
/// directly before `(` is a function or table name and stays tight.
const SPACED_BEFORE_PAREN: [&str; 30] = [
    "VALUES", "VALUE", "KEY", "IN", "ON", "AS", "AND", "OR", "NOT", "EXISTS", "BY", "USING",
    "SELECT", "WHERE", "FROM", "JOIN", "INTO", "TABLE", "INDEX", "UNIQUE", "DEFAULT", "CHECK",
    "THEN", "ELSE", "WHEN", "WITH", "SET", "IS", "LIKE", "BETWEEN",
];

/// The significant tokens of `tokens`, each with its parenthesis depth
/// (a `(` and its `)` sit at the depth outside them).
fn significant(tokens: &[Tok]) -> Vec<(Tok, usize)> {
    let mut depth = 0usize;
    let mut out = Vec::new();
    for t in tokens.iter().filter(|t| t.significant()) {
        if t.is_punct(')') {
            depth = depth.saturating_sub(1);
        }
        out.push((t.clone(), depth));
        if t.is_punct('(') {
            depth += 1;
        }
    }
    out
}

/// The significant tokens of `tokens` separated by single spaces, keeping
/// `f(x)`, `a.b` and `a, b` tight.
fn respace(tokens: &[Tok]) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut prev: Option<&Tok> = None;
    for t in tokens.iter().filter(|t| t.significant()) {
        if let Some(p) = prev {
            let tight = p.is_punct('(')
                || p.is_punct('.')
                || t.is_punct(')')
                || t.is_punct(',')
                || t.is_punct('.')
                || (t.is_punct('(')
                    && p.kind == Kind::Word
                    && !SPACED_BEFORE_PAREN.iter().any(|kw| p.is_kw(kw)));
            if !tight {
                out.push(Tok::space());
            }
        }
        out.push(t.clone());
        prev = Some(t);
    }
    out
}

/// [`respace`] rendered as text.
fn join(tokens: &[Tok]) -> String {
    render(&respace(tokens))
}

/// Split significant tokens at commas of depth `depth`.
fn split_commas(tokens: &[(Tok, usize)], depth: usize) -> Vec<Vec<(Tok, usize)>> {
    let mut parts = vec![Vec::new()];
    for (t, d) in tokens {
        if t.is_punct(',') && *d == depth {
            parts.push(Vec::new());
        } else if let Some(last) = parts.last_mut() {
            last.push((t.clone(), *d));
        }
    }
    parts
}

fn toks(items: &[(Tok, usize)]) -> Vec<Tok> {
    items.iter().map(|(t, _)| t.clone()).collect()
}

fn unquote_ident(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('`') && text.ends_with('`') {
        text[1..text.len() - 1].replace("``", "`")
    } else {
        text.to_string()
    }
}

fn is_ident(t: &Tok) -> bool {
    t.kind == Kind::Word || (t.kind == Kind::Quoted && t.text.starts_with('`'))
}

/// A `name` or `schema.name` starting at `sig[i]`; returns it and the index
/// after it.
fn table_at(sig: &[(Tok, usize)], i: usize) -> Option<(TableRef, usize)> {
    let first = &sig.get(i)?.0;
    if !is_ident(first) {
        return None;
    }
    if sig.get(i + 1).is_some_and(|(t, _)| t.is_punct('.')) {
        let second = &sig.get(i + 2)?.0;
        if !is_ident(second) {
            return None;
        }
        return Some((
            TableRef {
                schema: Some(unquote_ident(&first.text)),
                name: unquote_ident(&second.text),
                sql: format!("{}.{}", first.text, second.text),
            },
            i + 3,
        ));
    }
    Some((
        TableRef {
            schema: None,
            name: unquote_ident(&first.text),
            sql: first.text.clone(),
        },
        i + 1,
    ))
}

// ============================================================================
// Statement translation
// ============================================================================

fn translate_statement(
    tokens: Vec<Tok>,
    param_count: usize,
    positional: bool,
) -> Result<Statement, String> {
    let tokens = quote_reserved_names(tokens);
    let tokens = rewrite_ilike(tokens);
    let tokens = rewrite_intervals(tokens);
    let tokens = rewrite_limit_all(tokens);
    let tokens = rewrite_nulls_order(tokens);

    let sig = significant(&tokens);
    let first = sig.first().map(|(t, _)| t.text.to_ascii_uppercase());
    let (tokens, action) = match first.as_deref() {
        Some("INSERT") | Some("REPLACE") => translate_insert(tokens)?,
        Some("UPDATE") | Some("DELETE") => {
            if sig.iter().any(|(t, d)| *d == 0 && t.is_kw("RETURNING")) {
                return Err(format!(
                    "{} ... RETURNING is not supported on MySQL; read the rows with a SELECT",
                    first.unwrap_or_default()
                ));
            }
            (tokens, Action::Run)
        }
        Some("WITH") => {
            if sig.iter().any(|(t, _)| t.is_kw("RETURNING")) {
                return Err(
                    "WITH ... RETURNING (a data-modifying CTE) is not supported on MySQL"
                        .to_string(),
                );
            }
            (tokens, Action::Run)
        }
        Some("CREATE") => translate_create(tokens),
        Some("ALTER") => (translate_alter(tokens), Action::Run),
        Some("DROP") => translate_drop(tokens),
        _ => (tokens, Action::Run),
    };

    let (sql, binds) = number(&tokens, param_count, positional)?;
    Ok(Statement { sql, binds, action })
}

/// Render `tokens` with `?` placeholders and the parameter each binds.
/// `positional`: the placeholders were `?` (numbered by [`translate`]).
fn number(
    tokens: &[Tok],
    param_count: usize,
    positional: bool,
) -> Result<(String, Vec<usize>), String> {
    let mut sql = String::new();
    let mut binds = Vec::new();
    for t in tokens {
        match t.kind {
            Kind::Param(n) => {
                // `?` placeholders are checked by `translate`, which knows
                // whether the statement binds its parameters as given.
                if !positional && (n == 0 || n > param_count) {
                    return Err(format!(
                        "placeholder {} has no value ({} parameter{} given)",
                        t.text,
                        param_count,
                        if param_count == 1 { "" } else { "s" }
                    ));
                }
                sql.push('?');
                binds.push(n.saturating_sub(1));
            }
            _ => sql.push_str(&t.text),
        }
    }
    Ok((sql, binds))
}

/// Words MySQL 8 reserves that PostgreSQL and SQLite accept as plain
/// column or table names (a frame.data field `rank` or `key`).
const MYSQL_RESERVED_NAMES: [&str; 32] = [
    "KEY",
    "KEYS",
    "INDEX",
    "RANK",
    "DENSE_RANK",
    "PERCENT_RANK",
    "CUME_DIST",
    "ROW_NUMBER",
    "NTILE",
    "LAG",
    "LEAD",
    "FIRST_VALUE",
    "LAST_VALUE",
    "NTH_VALUE",
    "GROUPS",
    "SYSTEM",
    "READ",
    "WRITE",
    "CONDITION",
    "RANGE",
    "ROWS",
    "ROW",
    "SIGNAL",
    "USAGE",
    "LINES",
    "LONG",
    "SCHEMA",
    "SQL",
    "TRIGGER",
    "RELEASE",
    "EMPTY",
    "CHANGE",
];

/// Column types: a reserved word followed by one starts a column definition.
const COLUMN_TYPES: [&str; 24] = [
    "INTEGER",
    "INT",
    "BIGINT",
    "SMALLINT",
    "TINYINT",
    "REAL",
    "DOUBLE",
    "FLOAT",
    "DECIMAL",
    "NUMERIC",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "BOOLEAN",
    "BOOL",
    "TIMESTAMP",
    "DATETIME",
    "DATE",
    "TIME",
    "JSON",
    "UUID",
    "SERIAL",
    "BIGSERIAL",
    "BLOB",
];

/// Keywords after which a word names a column or table.
const BEFORE_NAME: [&str; 15] = [
    "BY", "SET", "SELECT", "WHERE", "AND", "OR", "NOT", "ON", "AS", "COLUMN", "TO", "DISTINCT",
    "WHEN", "THEN", "ELSE",
];

/// Backtick-quote [`MYSQL_RESERVED_NAMES`] where they stand for a name: not
/// a function call (`RANK()`, `KEY (cols)`), not a keyword after
/// PRIMARY/FOREIGN/UNIQUE/DUPLICATE, not `KEY idx (cols)`.
fn quote_reserved_names(mut tokens: Vec<Tok>) -> Vec<Tok> {
    let positions: Vec<usize> = (0..tokens.len())
        .filter(|&i| tokens[i].significant())
        .collect();
    for (k, &i) in positions.iter().enumerate() {
        let t = &tokens[i];
        if t.kind != Kind::Word || !MYSQL_RESERVED_NAMES.iter().any(|w| t.is_kw(w)) {
            continue;
        }
        let at = |n: usize| positions.get(n).map(|&p| &tokens[p]);
        let prev = k.checked_sub(1).and_then(at);
        let next = at(k + 1);
        let next2 = at(k + 2);
        if next.is_some_and(|n| n.is_punct('(')) {
            continue;
        }
        if prev.is_some_and(|p| {
            [
                "PRIMARY",
                "FOREIGN",
                "UNIQUE",
                "DUPLICATE",
                "FULLTEXT",
                "SPATIAL",
                "CREATE",
                "DROP",
                "USE",
                "FORCE",
                "IGNORE",
            ]
            .iter()
            .any(|kw| p.is_kw(kw))
        }) {
            continue;
        }
        let next_is_type = next.is_some_and(|n| COLUMN_TYPES.iter().any(|ty| n.is_kw(ty)));
        let index_definition = next.is_some_and(|n| n.kind == Kind::Word)
            && next2.is_some_and(|n| n.is_punct('('))
            && !next_is_type;
        if index_definition {
            continue;
        }
        let name = prev.is_some_and(|p| p.is_punct('.'))
            || next.is_none()
            || next.is_some_and(|n| {
                n.kind == Kind::Punct && !n.is_punct('(')
                    || n.is_kw("ASC")
                    || n.is_kw("DESC")
                    || n.is_kw("IS")
                    || n.is_kw("IN")
                    || n.is_kw("LIKE")
                    || n.is_kw("BETWEEN")
                    || n.is_kw("FROM")
                    || n.is_kw("AS")
            })
            || next_is_type
            || prev.is_some_and(|p| {
                p.is_punct('(') || p.is_punct(',') || BEFORE_NAME.iter().any(|kw| p.is_kw(kw))
            });
        if name {
            let quoted = format!("`{}`", tokens[i].text);
            tokens[i] = Tok {
                kind: Kind::Quoted,
                text: quoted,
            };
        }
    }
    tokens
}

/// `ILIKE` → `LIKE`: MySQL's default collations compare case-insensitively.
fn rewrite_ilike(tokens: Vec<Tok>) -> Vec<Tok> {
    tokens
        .into_iter()
        .map(|t| {
            if t.is_kw("ILIKE") {
                Tok::word("LIKE")
            } else {
                t
            }
        })
        .collect()
}

/// `INTERVAL '<n> <unit>'` → `INTERVAL <n> <UNIT>`.
fn rewrite_intervals(mut tokens: Vec<Tok>) -> Vec<Tok> {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_kw("INTERVAL") {
            let next = (i + 1..tokens.len()).find(|&j| tokens[j].significant());
            if let Some(j) = next {
                if let Some(mysql) = interval_literal(&tokens[j]) {
                    tokens[j] = Tok::word(&mysql);
                }
            }
        }
        i += 1;
    }
    tokens
}

fn interval_literal(t: &Tok) -> Option<String> {
    if t.kind != Kind::Quoted || !t.text.starts_with('\'') || t.text.len() < 2 {
        return None;
    }
    let inner = &t.text[1..t.text.len() - 1];
    let mut parts = inner.split_whitespace();
    let amount = parts.next()?;
    let unit = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let numeric = amount.trim_start_matches('-');
    if numeric.is_empty() || !numeric.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let unit = match unit.to_ascii_lowercase().as_str() {
        "microsecond" | "microseconds" => "MICROSECOND",
        "second" | "seconds" | "sec" | "secs" => "SECOND",
        "minute" | "minutes" | "min" | "mins" => "MINUTE",
        "hour" | "hours" => "HOUR",
        "day" | "days" => "DAY",
        "week" | "weeks" => "WEEK",
        "month" | "months" => "MONTH",
        "year" | "years" => "YEAR",
        _ => return None,
    };
    Some(format!("{} {}", amount, unit))
}

/// `LIMIT ALL` → the largest MySQL row count.
fn rewrite_limit_all(mut tokens: Vec<Tok>) -> Vec<Tok> {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_kw("LIMIT") {
            if let Some(j) = (i + 1..tokens.len()).find(|&j| tokens[j].significant()) {
                if tokens[j].is_kw("ALL") {
                    tokens[j] = Tok::word("18446744073709551615");
                }
            }
        }
        i += 1;
    }
    tokens
}

/// `ORDER BY <expr> [ASC|DESC] NULLS FIRST|LAST` → `(<expr>) IS NULL
/// DESC|ASC, <expr> [ASC|DESC]` (MySQL sorts NULL first ascending).
fn rewrite_nulls_order(tokens: Vec<Tok>) -> Vec<Tok> {
    let mut tokens = tokens;
    loop {
        let sig = significant(&tokens);
        let Some(at) = (0..sig.len()).find(|&k| {
            sig[k].0.is_kw("NULLS")
                && sig
                    .get(k + 1)
                    .is_some_and(|(t, _)| t.is_kw("FIRST") || t.is_kw("LAST"))
        }) else {
            return tokens;
        };
        let depth = sig[at].1;
        let mut start = at;
        while start > 0 {
            let (t, d) = &sig[start - 1];
            if *d < depth || (*d == depth && (t.is_punct(',') || t.is_kw("BY"))) {
                break;
            }
            start -= 1;
        }
        let mut expr_end = at;
        let mut direction = String::new();
        if expr_end > start
            && (sig[expr_end - 1].0.is_kw("ASC") || sig[expr_end - 1].0.is_kw("DESC"))
        {
            direction = format!(" {}", sig[expr_end - 1].0.text.to_ascii_uppercase());
            expr_end -= 1;
        }
        let expr = join(&toks(&sig[start..expr_end]));
        let nulls_first = sig[at + 1].0.is_kw("FIRST");
        let item = format!(
            "({}) IS NULL {}, {}{}",
            expr,
            if nulls_first { "DESC" } else { "ASC" },
            expr,
            direction
        );
        let mut rebuilt = toks(&sig[..start]);
        rebuilt.extend(lex(&item));
        rebuilt.extend(toks(&sig[at + 2..]));
        tokens = respace(&rebuilt);
    }
}

// ---------------------------------------------------------------- INSERT ---

fn translate_insert(tokens: Vec<Tok>) -> Result<(Vec<Tok>, Action), String> {
    let sig = significant(&tokens);
    // INSERT [LOW_PRIORITY | DELAYED | HIGH_PRIORITY] [IGNORE] [INTO] <table>
    let mut i = 1;
    while sig.get(i).is_some_and(|(t, _)| {
        t.is_kw("LOW_PRIORITY")
            || t.is_kw("DELAYED")
            || t.is_kw("HIGH_PRIORITY")
            || t.is_kw("IGNORE")
    }) {
        i += 1;
    }
    if sig.get(i).is_some_and(|(t, _)| t.is_kw("INTO")) {
        i += 1;
    }
    let Some((table, after_table)) = table_at(&sig, i) else {
        return Ok((tokens, Action::Run));
    };

    let returning_at = sig
        .iter()
        .position(|(t, d)| *d == 0 && t.is_kw("RETURNING"));
    let conflict_at = (0..sig.len()).find(|&k| {
        sig[k].1 == 0
            && sig[k].0.is_kw("ON")
            && sig.get(k + 1).is_some_and(|(t, _)| t.is_kw("CONFLICT"))
    });
    if returning_at.is_some() && conflict_at.is_some() {
        return Err("INSERT ... ON CONFLICT ... RETURNING is not supported on MySQL".to_string());
    }

    let mut action = Action::Run;
    if let Some(c) = conflict_at {
        return Ok((translate_on_conflict(&tokens, &sig, c)?, action));
    }

    let mut body = tokens;
    if let Some(r) = returning_at {
        action = Action::InsertReturning {
            table: table.clone(),
            columns: parse_returning(&sig[r + 1..])?,
        };
        let cut = position_of(&body, r);
        body.truncate(cut);
        body = trim(body);
    }

    // INSERT INTO t DEFAULT VALUES → INSERT INTO t () VALUES ()
    if sig
        .get(after_table)
        .is_some_and(|(t, _)| t.is_kw("DEFAULT"))
        && sig
            .get(after_table + 1)
            .is_some_and(|(t, _)| t.is_kw("VALUES"))
    {
        let from = position_of(&body, after_table);
        let to = position_of(&body, after_table + 1) + 1;
        body.splice(from..to, lex("() VALUES ()"));
    }
    Ok((body, action))
}

fn parse_returning(items: &[(Tok, usize)]) -> Result<Returning, String> {
    if items.len() == 1 && items[0].0.is_punct('*') {
        return Ok(Returning::All);
    }
    let mut columns = Vec::new();
    for item in split_commas(items, 0) {
        let t: Vec<&Tok> = item.iter().map(|(t, _)| t).collect();
        // <col> | <qualifier>.<col>, then [AS] <alias>
        let (column, rest) = match t.as_slice() {
            [q, dot, c, rest @ ..] if is_ident(q) && dot.is_punct('.') && is_ident(c) => {
                (unquote_ident(&c.text), rest)
            }
            [c, rest @ ..] if is_ident(c) => (unquote_ident(&c.text), rest),
            _ => return Err(unsupported_returning(items)),
        };
        let label = match rest {
            [] => column.clone(),
            [kw, alias] if kw.is_kw("AS") && is_ident(alias) => unquote_ident(&alias.text),
            [alias] if is_ident(alias) => unquote_ident(&alias.text),
            _ => return Err(unsupported_returning(items)),
        };
        columns.push(ReturnColumn { column, label });
    }
    if columns.is_empty() {
        return Err(unsupported_returning(items));
    }
    Ok(Returning::Columns(columns))
}

fn unsupported_returning(items: &[(Tok, usize)]) -> String {
    format!(
        "RETURNING {} is not supported on MySQL: return `*` or column names",
        join(&toks(items))
    )
}

/// `ON CONFLICT [(cols) | ON CONSTRAINT c] DO NOTHING | DO UPDATE SET ...`
/// (starting at `sig[at]`) → `ON DUPLICATE KEY UPDATE ...`.
fn translate_on_conflict(
    tokens: &[Tok],
    sig: &[(Tok, usize)],
    at: usize,
) -> Result<Vec<Tok>, String> {
    let mut i = at + 2;
    let mut conflict_columns = Vec::new();
    if sig.get(i).is_some_and(|(t, _)| t.is_punct('(')) {
        let close = (i + 1..sig.len())
            .find(|&k| sig[k].1 == 0 && sig[k].0.is_punct(')'))
            .ok_or("ON CONFLICT column list is not closed")?;
        for item in split_commas(&sig[i + 1..close], 1) {
            if let [(t, _)] = item.as_slice() {
                if is_ident(t) {
                    conflict_columns.push(t.text.clone());
                }
            }
        }
        i = close + 1;
    } else if sig.get(i).is_some_and(|(t, _)| t.is_kw("ON"))
        && sig.get(i + 1).is_some_and(|(t, _)| t.is_kw("CONSTRAINT"))
    {
        i += 3;
    }
    if !sig.get(i).is_some_and(|(t, _)| t.is_kw("DO")) {
        return Err("ON CONFLICT needs DO NOTHING or DO UPDATE".to_string());
    }
    let head = trim(tokens[..position_of(tokens, at)].to_vec());
    if sig.get(i + 1).is_some_and(|(t, _)| t.is_kw("NOTHING")) {
        if i + 2 != sig.len() {
            return Err("unexpected SQL after ON CONFLICT ... DO NOTHING".to_string());
        }
        return Ok(match conflict_columns.first() {
            Some(col) => {
                let mut out = head;
                out.extend(lex(&format!(" ON DUPLICATE KEY UPDATE {} = {}", col, col)));
                out
            }
            None => {
                // No conflict target to name: ignore duplicate-key rows.
                let mut out = vec![Tok::word("INSERT"), Tok::space(), Tok::word("IGNORE")];
                out.extend(head.into_iter().skip(1));
                out
            }
        });
    }
    if !(sig.get(i + 1).is_some_and(|(t, _)| t.is_kw("UPDATE"))
        && sig.get(i + 2).is_some_and(|(t, _)| t.is_kw("SET")))
    {
        return Err("ON CONFLICT needs DO NOTHING or DO UPDATE SET".to_string());
    }
    let assignments = &sig[i + 3..];
    if assignments.iter().any(|(t, d)| *d == 0 && t.is_kw("WHERE")) {
        return Err("ON CONFLICT ... DO UPDATE ... WHERE is not supported on MySQL".to_string());
    }
    // EXCLUDED.<col> is the proposed row: MySQL names it with a row alias.
    let mut uses_excluded = false;
    let mut set: Vec<Tok> = Vec::new();
    for (k, (t, _)) in assignments.iter().enumerate() {
        if t.is_kw("EXCLUDED") && assignments.get(k + 1).is_some_and(|(n, _)| n.is_punct('.')) {
            uses_excluded = true;
            set.push(Tok::word("__excluded"));
        } else {
            set.push(t.clone());
        }
    }
    let mut out = head;
    let set = respace(&set);
    if uses_excluded {
        let values_source = sig[..at]
            .iter()
            .any(|(t, d)| *d == 0 && (t.is_kw("VALUES") || t.is_kw("VALUE")));
        if !values_source {
            return Err(
                "ON CONFLICT ... EXCLUDED.<column> needs INSERT ... VALUES on MySQL".to_string(),
            );
        }
        out.extend(lex(" AS __excluded"));
    }
    out.extend(lex(" ON DUPLICATE KEY UPDATE "));
    out.extend(set);
    Ok(out)
}

// ---------------------------------------------------------------- CREATE ---

fn translate_create(tokens: Vec<Tok>) -> (Vec<Tok>, Action) {
    let sig = significant(&tokens);
    let second = sig.get(1).map(|(t, _)| t.text.to_ascii_uppercase());
    match second.as_deref() {
        Some("INDEX") | Some("UNIQUE") => match parse_create_index(&sig) {
            Some(spec) => (tokens, Action::CreateIndex(spec)),
            None => (tokens, Action::Run),
        },
        Some("TABLE") | Some("TEMPORARY") => {
            (translate_create_table(&sig).unwrap_or(tokens), Action::Run)
        }
        _ => (tokens, Action::Run),
    }
}

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] <name> ON <table> (<cols>) [tail]`.
fn parse_create_index(sig: &[(Tok, usize)]) -> Option<IndexSpec> {
    let mut i = 1;
    let unique = sig.get(i)?.0.is_kw("UNIQUE");
    if unique {
        i += 1;
    }
    if !sig.get(i)?.0.is_kw("INDEX") {
        return None;
    }
    i += 1;
    let mut if_not_exists = false;
    if sig.get(i)?.0.is_kw("IF")
        && sig.get(i + 1)?.0.is_kw("NOT")
        && sig.get(i + 2)?.0.is_kw("EXISTS")
    {
        if_not_exists = true;
        i += 3;
    }
    let name = &sig.get(i)?.0;
    if !is_ident(name) || !sig.get(i + 1)?.0.is_kw("ON") {
        return None;
    }
    let (table, after) = table_at(sig, i + 2)?;
    if !sig.get(after)?.0.is_punct('(') {
        return None;
    }
    let close = (after + 1..sig.len()).find(|&k| sig[k].1 == 0 && sig[k].0.is_punct(')'))?;
    let mut columns = Vec::new();
    for item in split_commas(&sig[after + 1..close], 1) {
        let mut t = toks(&item);
        let mut suffix = String::new();
        if t.last().is_some_and(|l| l.is_kw("ASC") || l.is_kw("DESC")) {
            suffix = format!(" {}", t.pop()?.text.to_ascii_uppercase());
        }
        let column = match t.as_slice() {
            [c] if is_ident(c) => Some(unquote_ident(&c.text)),
            _ => None,
        };
        columns.push(IndexColumn {
            column,
            sql: join(&t),
            suffix,
        });
    }
    Some(IndexSpec {
        unique,
        if_not_exists,
        name: name.text.clone(),
        table,
        columns,
        tail: join(&toks(&sig[close + 1..])),
    })
}

/// Rewrite the column definitions of `CREATE TABLE <name> (...)`; `None`
/// when none needs it or the statement has another shape (`AS SELECT`).
fn translate_create_table(sig: &[(Tok, usize)]) -> Option<Vec<Tok>> {
    let open = sig.iter().position(|(t, d)| *d == 0 && t.is_punct('('))?;
    let close = (open + 1..sig.len()).find(|&k| sig[k].1 == 0 && sig[k].0.is_punct(')'))?;
    let mut defs = Vec::new();
    let mut foreign_keys = Vec::new();
    let mut changed = false;
    for item in split_commas(&sig[open + 1..close], 1) {
        let first = item.first().map(|(t, _)| t.text.to_ascii_uppercase());
        let is_table_constraint = matches!(
            first.as_deref(),
            Some(
                "CONSTRAINT"
                    | "PRIMARY"
                    | "UNIQUE"
                    | "KEY"
                    | "INDEX"
                    | "FOREIGN"
                    | "CHECK"
                    | "FULLTEXT"
                    | "SPATIAL"
            )
        );
        if is_table_constraint || item.len() < 2 {
            defs.push(join(&toks(&item)));
            continue;
        }
        let (def, fk) = translate_column_def(&item);
        changed |= fk.is_some() || def != join(&toks(&item));
        defs.push(def);
        foreign_keys.extend(fk);
    }
    if !changed {
        return None;
    }
    defs.extend(foreign_keys);
    let sql = format!(
        "{} ({}){}",
        join(&toks(&sig[..open])),
        defs.join(", "),
        tail_sql(&sig[close + 1..])
    );
    Some(lex(&sql))
}

fn tail_sql(items: &[(Tok, usize)]) -> String {
    if items.is_empty() {
        String::new()
    } else {
        format!(" {}", join(&toks(items)))
    }
}

/// One column definition for MySQL; the second value is the table-level
/// `FOREIGN KEY` an inline `REFERENCES` becomes (MySQL parses inline
/// `REFERENCES` but ignores it).
fn translate_column_def(item: &[(Tok, usize)]) -> (String, Option<String>) {
    let base = item[0].1;
    let name = item[0].0.text.clone();
    let mut i = 1;
    let mut ty = toks(&item[i..i + 1]);
    i += 1;
    if item
        .get(i)
        .is_some_and(|(t, d)| *d == base && t.is_punct('('))
    {
        if let Some(close) =
            (i + 1..item.len()).find(|&k| item[k].1 == base && item[k].0.is_punct(')'))
        {
            ty.extend(toks(&item[i..=close]));
            i = close + 1;
        }
    }
    let mut rest = toks(&item[i..]);

    // Inline REFERENCES → table-level FOREIGN KEY.
    let mut foreign_key = None;
    if let Some(r) = rest.iter().position(|t| t.is_kw("REFERENCES")) {
        let mut end = r + 1;
        if rest.get(end).is_some_and(is_ident) {
            end += 1;
            if rest.get(end).is_some_and(|t| t.is_punct('.'))
                && rest.get(end + 1).is_some_and(is_ident)
            {
                end += 2;
            }
        }
        if rest.get(end).is_some_and(|t| t.is_punct('(')) {
            if let Some(close) = (end..rest.len()).find(|&k| rest[k].is_punct(')')) {
                end = close + 1;
            }
        }
        loop {
            let at = |k: usize, kw: &str| rest.get(k).is_some_and(|t| t.is_kw(kw));
            if at(end, "ON") && (at(end + 1, "DELETE") || at(end + 1, "UPDATE")) {
                if at(end + 2, "SET") || at(end + 2, "NO") {
                    end += 4;
                } else {
                    end += 3;
                }
            } else if at(end, "MATCH") {
                end += 2;
            } else {
                break;
            }
        }
        let end = end.min(rest.len());
        let clause: Vec<Tok> = rest.drain(r..end).collect();
        foreign_key = Some(format!("FOREIGN KEY ({}) {}", name, join(&clause)));
    }

    let has = |rest: &[Tok], kw: &str| rest.iter().any(|t| t.is_kw(kw));
    let primary = has(&rest, "PRIMARY");
    let unique = has(&rest, "UNIQUE");
    let keyed = primary || unique;
    let type_name = ty[0].text.to_ascii_uppercase();
    let mut auto_increment = false;
    let mysql_type = match type_name.as_str() {
        "SERIAL" | "BIGSERIAL" => {
            auto_increment = true;
            "BIGINT".to_string()
        }
        "SMALLSERIAL" => {
            auto_increment = true;
            "SMALLINT".to_string()
        }
        // Clean integers are 64-bit, and a key column and the foreign keys
        // that reference it must have the same type.
        "INTEGER" if ty.len() == 1 => "BIGINT".to_string(),
        "UUID" if ty.len() == 1 => "CHAR(36)".to_string(),
        // MySQL keys a TEXT column only by prefix.
        "TEXT" if ty.len() == 1 && keyed => format!("VARCHAR({})", TEXT_KEY_PREFIX),
        _ => join(&ty),
    };

    // TEXT / BLOB / JSON take a default only as an expression: DEFAULT ('x').
    let upper = mysql_type.to_ascii_uppercase();
    let expression_default = upper.ends_with("TEXT") || upper.ends_with("BLOB") || upper == "JSON";
    if expression_default {
        if let Some(d) = rest.iter().position(|t| t.is_kw("DEFAULT")) {
            if let Some(v) = rest.get(d + 1) {
                let literal = v.kind == Kind::Quoted && v.text.starts_with('\'')
                    || (v.kind == Kind::Word && !v.is_kw("NULL"));
                if literal {
                    let wrapped = lex(&format!("({})", v.text));
                    rest.splice(d + 1..d + 2, wrapped);
                }
            }
        }
    }

    let mut parts = vec![name, mysql_type];
    if auto_increment {
        if !primary {
            // MySQL's own SERIAL: an AUTO_INCREMENT column must be a key.
            parts.push("NOT NULL".to_string());
        }
        parts.push("AUTO_INCREMENT".to_string());
        if !keyed {
            parts.push("UNIQUE".to_string());
        }
    }
    let rest_sql = join(&rest);
    if !rest_sql.is_empty() {
        parts.push(rest_sql);
    }
    (parts.join(" "), foreign_key)
}

// ----------------------------------------------------------------- ALTER ---

/// `ALTER TABLE <t> ADD [COLUMN] <def>, ...`: column definitions as in
/// CREATE TABLE.
fn translate_alter(tokens: Vec<Tok>) -> Vec<Tok> {
    let sig = significant(&tokens);
    if !sig.get(1).is_some_and(|(t, _)| t.is_kw("TABLE")) {
        return tokens;
    }
    let Some((_, after)) = table_at(&sig, 2) else {
        return tokens;
    };
    let mut actions = Vec::new();
    let mut changed = false;
    for action in split_commas(&sig[after..], 0) {
        let is_add_column = action.first().is_some_and(|(t, _)| t.is_kw("ADD"))
            && !action.get(1).is_some_and(|(t, _)| {
                [
                    "CONSTRAINT",
                    "PRIMARY",
                    "UNIQUE",
                    "INDEX",
                    "KEY",
                    "FOREIGN",
                    "CHECK",
                    "FULLTEXT",
                    "SPATIAL",
                    "PARTITION",
                ]
                .iter()
                .any(|kw| t.is_kw(kw))
            });
        if !is_add_column {
            actions.push(join(&toks(&action)));
            continue;
        }
        let def_start = if action.get(1).is_some_and(|(t, _)| t.is_kw("COLUMN")) {
            2
        } else {
            1
        };
        if action.len() < def_start + 2 {
            actions.push(join(&toks(&action)));
            continue;
        }
        let (def, fk) = translate_column_def(&action[def_start..]);
        actions.push(format!("{} {}", join(&toks(&action[..def_start])), def));
        if let Some(fk) = fk {
            actions.push(format!("ADD {}", fk));
        }
        changed = true;
    }
    if !changed {
        return tokens;
    }
    lex(&format!(
        "{} {}",
        join(&toks(&sig[..after])),
        actions.join(", ")
    ))
}

// ------------------------------------------------------------------ DROP ---

/// `DROP INDEX [IF EXISTS] <name> [ON <table>]`.
fn translate_drop(tokens: Vec<Tok>) -> (Vec<Tok>, Action) {
    let sig = significant(&tokens);
    if !sig.get(1).is_some_and(|(t, _)| t.is_kw("INDEX")) {
        return (tokens, Action::Run);
    }
    let mut i = 2;
    let mut if_exists = false;
    if sig.get(i).is_some_and(|(t, _)| t.is_kw("IF"))
        && sig.get(i + 1).is_some_and(|(t, _)| t.is_kw("EXISTS"))
    {
        if_exists = true;
        i += 2;
    }
    let Some((name, _)) = sig.get(i) else {
        return (tokens, Action::Run);
    };
    if !is_ident(name) {
        return (tokens, Action::Run);
    }
    let table = if sig.get(i + 1).is_some_and(|(t, _)| t.is_kw("ON")) {
        match table_at(&sig, i + 2) {
            Some((t, end)) if end == sig.len() => Some(t),
            _ => return (tokens, Action::Run),
        }
    } else if i + 1 == sig.len() {
        None
    } else {
        return (tokens, Action::Run);
    };
    (
        tokens,
        Action::DropIndex {
            name: name.text.clone(),
            table,
            if_exists,
        },
    )
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str, params: usize) -> Statement {
        let mut s = translate(sql, params).expect("translates");
        assert_eq!(s.len(), 1, "one statement expected: {:?}", s);
        s.remove(0)
    }

    fn run_sql(sql: &str) -> String {
        let s = one(sql, 0);
        assert_eq!(s.action, Action::Run);
        s.sql
    }

    // ------------------------------------------------------- placeholders

    #[test]
    fn numbered_placeholders_become_question_marks_in_order() {
        let s = one("SELECT * FROM users WHERE email = $1 AND name = $2", 2);
        assert_eq!(s.sql, "SELECT * FROM users WHERE email = ? AND name = ?");
        assert_eq!(s.binds, vec![0, 1]);
        assert_eq!(s.action, Action::Run);
    }

    #[test]
    fn out_of_order_placeholders_bind_by_number() {
        // frame.data's save(): SET columns are $2.., the key is $1.
        let s = one("UPDATE users SET email = $2, name = $3 WHERE id = $1", 3);
        assert_eq!(s.sql, "UPDATE users SET email = ?, name = ? WHERE id = ?");
        assert_eq!(s.binds, vec![1, 2, 0]);
    }

    #[test]
    fn a_repeated_placeholder_binds_its_value_each_time() {
        let s = one("SELECT * FROM t WHERE a = $1 OR b = $1 OR c = $2", 2);
        assert_eq!(s.sql, "SELECT * FROM t WHERE a = ? OR b = ? OR c = ?");
        assert_eq!(s.binds, vec![0, 0, 1]);
    }

    #[test]
    fn dollars_inside_literals_and_identifiers_are_text() {
        let s = one(
            "SELECT JSON_EXTRACT(meta, '$.a$1') AS `x$1`, \"$2\" FROM t WHERE a$1 = $1 -- $9\n",
            1,
        );
        assert_eq!(
            s.sql,
            "SELECT JSON_EXTRACT(meta, '$.a$1') AS `x$1`, \"$2\" FROM t WHERE a$1 = ? -- $9"
        );
        assert_eq!(s.binds, vec![0]);
    }

    #[test]
    fn question_mark_sql_passes_through_with_its_parameters() {
        let sql = "SELECT p.slug FROM pages p WHERE p.slug = ? AND l.code = ? AND x = '?'";
        let s = one(sql, 2);
        assert_eq!(s.sql, sql);
        assert_eq!(s.binds, vec![0, 1]);
    }

    #[test]
    fn mixing_placeholder_styles_is_an_error() {
        let err = translate("SELECT * FROM t WHERE a = ? AND b = $1", 2).unwrap_err();
        assert!(err.contains("mixes"), "{}", err);
    }

    #[test]
    fn a_placeholder_without_a_value_is_an_error() {
        let err = translate("SELECT * FROM t WHERE a = $2", 1).unwrap_err();
        assert!(err.contains("$2"), "{}", err);
        assert!(translate("SELECT * FROM t WHERE a = $0", 1).is_err());
    }

    #[test]
    fn statements_split_at_semicolons_each_with_its_binds() {
        let s = translate(
            "CREATE TABLE a (id INTEGER); INSERT INTO a (id) VALUES ($2); SELECT ';' FROM a WHERE id = $1;",
            2,
        )
        .unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].sql, "CREATE TABLE a (id BIGINT)");
        assert_eq!(s[1].sql, "INSERT INTO a (id) VALUES (?)");
        assert_eq!(s[1].binds, vec![1]);
        assert_eq!(s[2].sql, "SELECT ';' FROM a WHERE id = ?");
        assert_eq!(s[2].binds, vec![0]);
    }

    #[test]
    fn positional_placeholders_across_statements_bind_in_sequence() {
        let s = translate("INSERT INTO a VALUES (?); INSERT INTO b VALUES (?, ?)", 3).unwrap();
        assert_eq!(s[0].binds, vec![0]);
        assert_eq!(s[1].binds, vec![1, 2]);
    }

    // ------------------------------------------------------------- INSERT

    #[test]
    fn insert_returning_runs_the_insert_and_names_the_columns() {
        let s = one(
            "INSERT INTO users (email, name) VALUES ($1, $2) RETURNING id",
            2,
        );
        assert_eq!(s.sql, "INSERT INTO users (email, name) VALUES (?, ?)");
        assert_eq!(s.binds, vec![0, 1]);
        match s.action {
            Action::InsertReturning { table, columns } => {
                assert_eq!(table.name, "users");
                assert_eq!(table.schema, None);
                assert_eq!(
                    columns,
                    Returning::Columns(vec![ReturnColumn {
                        column: "id".into(),
                        label: "id".into()
                    }])
                );
            }
            other => panic!("expected InsertReturning, got {:?}", other),
        }
    }

    #[test]
    fn insert_returning_star_aliases_and_schema() {
        let s = one("INSERT INTO app.posts (title) VALUES ($1) RETURNING *", 1);
        match &s.action {
            Action::InsertReturning { table, columns } => {
                assert_eq!(table.schema.as_deref(), Some("app"));
                assert_eq!(table.name, "posts");
                assert_eq!(table.sql, "app.posts");
                assert_eq!(*columns, Returning::All);
            }
            other => panic!("{:?}", other),
        }
        let s = one(
            "INSERT INTO posts (title) VALUES ($1) RETURNING id AS post_id, posts.created_at, `title` t",
            1,
        );
        let Action::InsertReturning { columns, .. } = s.action else {
            panic!("expected InsertReturning");
        };
        assert_eq!(
            columns,
            Returning::Columns(vec![
                ReturnColumn {
                    column: "id".into(),
                    label: "post_id".into()
                },
                ReturnColumn {
                    column: "created_at".into(),
                    label: "created_at".into()
                },
                ReturnColumn {
                    column: "title".into(),
                    label: "t".into()
                },
            ])
        );
    }

    #[test]
    fn insert_returning_an_expression_is_an_error() {
        let err = translate("INSERT INTO t (a) VALUES ($1) RETURNING id + 1", 1).unwrap_err();
        assert!(err.contains("RETURNING"), "{}", err);
    }

    #[test]
    fn insert_default_values_becomes_empty_values() {
        let s = one("INSERT INTO counters DEFAULT VALUES RETURNING id", 0);
        assert_eq!(s.sql, "INSERT INTO counters () VALUES ()");
        assert!(matches!(s.action, Action::InsertReturning { .. }));
        assert_eq!(
            run_sql("INSERT INTO counters DEFAULT VALUES"),
            "INSERT INTO counters () VALUES ()"
        );
    }

    #[test]
    fn on_conflict_do_update_uses_a_row_alias_for_excluded() {
        let s = one(
            "INSERT INTO kv (k, v) VALUES ($1, $2) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
            2,
        );
        assert_eq!(
            s.sql,
            "INSERT INTO kv (k, v) VALUES (?, ?) AS __excluded ON DUPLICATE KEY UPDATE v = __excluded.v"
        );
        assert_eq!(
            run_sql("INSERT INTO kv (k, v) VALUES (1, 2) ON CONFLICT (k) DO UPDATE SET v = 5"),
            "INSERT INTO kv (k, v) VALUES (1, 2) ON DUPLICATE KEY UPDATE v = 5"
        );
    }

    #[test]
    fn on_conflict_do_nothing() {
        assert_eq!(
            run_sql("INSERT INTO kv (k, v) VALUES (1, 2) ON CONFLICT (k) DO NOTHING"),
            "INSERT INTO kv (k, v) VALUES (1, 2) ON DUPLICATE KEY UPDATE k = k"
        );
        assert_eq!(
            run_sql("INSERT INTO kv (k, v) VALUES (1, 2) ON CONFLICT DO NOTHING"),
            "INSERT IGNORE INTO kv (k, v) VALUES (1, 2)"
        );
    }

    #[test]
    fn on_conflict_shapes_mysql_cannot_express_are_errors() {
        assert!(translate(
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET v = 1 WHERE kv.v < 1",
            0
        )
        .is_err());
        assert!(translate(
            "INSERT INTO kv (k) SELECT k FROM o ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
            0
        )
        .is_err());
        assert!(translate(
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO NOTHING RETURNING k",
            0
        )
        .is_err());
    }

    #[test]
    fn mysql_upserts_are_untouched() {
        let sql = "INSERT INTO kv (k, v) VALUES (?, ?) ON DUPLICATE KEY UPDATE v = VALUES(v)";
        assert_eq!(one(sql, 2).sql, sql);
    }

    // ---------------------------------------------------- UPDATE / DELETE

    #[test]
    fn update_and_delete_returning_are_errors() {
        assert!(translate("UPDATE t SET a = $1 RETURNING id", 1)
            .unwrap_err()
            .contains("UPDATE"));
        assert!(translate("DELETE FROM t WHERE id = $1 RETURNING *", 1)
            .unwrap_err()
            .contains("DELETE"));
        assert!(translate(
            "WITH m AS (DELETE FROM t RETURNING *) INSERT INTO u SELECT * FROM m",
            0
        )
        .is_err());
    }

    #[test]
    fn delete_with_tenant_scope_binds_in_order() {
        // frame.data's delete(): key $1, then the tenant scope's `?` as $2.
        let s = one(
            "DELETE FROM notes WHERE notes.tenantId = $2 AND (id = $1)",
            2,
        );
        assert_eq!(
            s.sql,
            "DELETE FROM notes WHERE notes.tenantId = ? AND (id = ?)"
        );
        assert_eq!(s.binds, vec![1, 0]);
    }

    // -------------------------------------------------------------- SELECT

    #[test]
    fn frame_data_selects_pass_through() {
        for sql in [
            "SELECT COUNT(*) AS count FROM users WHERE email = ?",
            "SELECT 1 AS present FROM users WHERE active = true LIMIT 1",
            "SELECT * FROM posts WHERE posts.id IN (SELECT posts.id FROM posts LEFT JOIN comments comments ON comments.postId = posts.id WHERE comments.body LIKE '%a%') ORDER BY title DESC LIMIT 10 OFFSET 20",
            "SELECT * FROM t LIMIT 9223372036854775807 OFFSET 5",
            "SELECT * FROM t WHERE JSON_EXTRACT(meta, '$.tags[0]') = 'x' AND flag = FALSE",
            "SELECT a || b AS ab FROM t",
        ] {
            assert_eq!(run_sql(sql), sql);
        }
    }

    #[test]
    fn interval_literals_take_mysql_units() {
        assert_eq!(
            run_sql("SELECT * FROM s WHERE expires > (NOW() + INTERVAL '3 days') AND seen < (NOW() - INTERVAL '90 minutes')"),
            "SELECT * FROM s WHERE expires > (NOW() + INTERVAL 3 DAY) AND seen < (NOW() - INTERVAL 90 MINUTE)"
        );
        // MySQL's own spelling stays.
        let native = "SELECT NOW() + INTERVAL '3' DAY";
        assert_eq!(run_sql(native), native);
    }

    #[test]
    fn ilike_becomes_like() {
        assert_eq!(
            run_sql("SELECT * FROM t WHERE name ILIKE '%ann%' AND x NOT ILIKE 'b'"),
            "SELECT * FROM t WHERE name LIKE '%ann%' AND x NOT LIKE 'b'"
        );
    }

    #[test]
    fn nulls_first_and_last_become_is_null_keys() {
        assert_eq!(
            run_sql("SELECT * FROM t ORDER BY score DESC NULLS LAST, name ASC NULLS FIRST, id"),
            "SELECT * FROM t ORDER BY (score) IS NULL ASC, score DESC, (name) IS NULL DESC, name ASC, id"
        );
        let s = one(
            "SELECT * FROM t ORDER BY COALESCE(a, $1) NULLS FIRST LIMIT 5",
            1,
        );
        assert_eq!(
            s.sql,
            "SELECT * FROM t ORDER BY (COALESCE(a, ?)) IS NULL DESC, COALESCE(a, ?) LIMIT 5"
        );
        assert_eq!(s.binds, vec![0, 0]);
    }

    #[test]
    fn limit_all_becomes_the_largest_row_count() {
        assert_eq!(
            run_sql("SELECT * FROM t LIMIT ALL OFFSET 3"),
            "SELECT * FROM t LIMIT 18446744073709551615 OFFSET 3"
        );
    }

    #[test]
    fn mysql_reserved_names_are_quoted_where_they_name_columns() {
        assert_eq!(
            run_sql("CREATE TABLE IF NOT EXISTS entries (id SERIAL PRIMARY KEY, rank INTEGER NOT NULL, key TEXT UNIQUE)"),
            "CREATE TABLE IF NOT EXISTS entries (id BIGINT AUTO_INCREMENT PRIMARY KEY, `rank` BIGINT NOT NULL, `key` VARCHAR(255) UNIQUE)"
        );
        let s = one(
            "SELECT * FROM entries WHERE entries.rank > $1 AND key = $2 ORDER BY rank ASC, entries.key LIMIT 5",
            2,
        );
        assert_eq!(
            s.sql,
            "SELECT * FROM entries WHERE entries.`rank` > ? AND `key` = ? ORDER BY `rank` ASC, entries.`key` LIMIT 5"
        );
        assert_eq!(
            one("UPDATE entries SET rank = $2, key = $3 WHERE id = $1", 3).sql,
            "UPDATE entries SET `rank` = ?, `key` = ? WHERE id = ?"
        );
        assert_eq!(
            one(
                "INSERT INTO entries (rank, key) VALUES ($1, $2) RETURNING id",
                2
            )
            .sql,
            "INSERT INTO entries (`rank`, `key`) VALUES (?, ?)"
        );
    }

    #[test]
    fn reserved_words_used_as_keywords_stay() {
        for sql in [
            "SELECT name, RANK() OVER (ORDER BY score DESC) AS r FROM t",
            "CREATE TABLE t (id INT, name VARCHAR(10), PRIMARY KEY (id), KEY idx_name (name), UNIQUE KEY uq (name), INDEX idx2 (id))",
            "INSERT INTO t (id) VALUES (1) ON DUPLICATE KEY UPDATE id = id",
            "SELECT * FROM t FORCE INDEX (idx) WHERE a = 1",
        ] {
            assert_eq!(run_sql(sql), sql);
        }
        let Action::CreateIndex(spec) = one("CREATE INDEX idx ON t (key)", 0).action else {
            panic!("expected CreateIndex");
        };
        assert_eq!(spec.columns[0].sql, "`key`");
        assert_eq!(spec.columns[0].column.as_deref(), Some("key"));
    }

    // ---------------------------------------------------------------- DDL

    #[test]
    fn create_table_with_a_serial_key() {
        assert_eq!(
            run_sql("CREATE TABLE IF NOT EXISTS users (id SERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT NOT NULL, age INTEGER, score REAL, active BOOLEAN DEFAULT true, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP, born DATE, token UUID, meta JSON)"),
            "CREATE TABLE IF NOT EXISTS users (id BIGINT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(255) NOT NULL UNIQUE, name TEXT NOT NULL, age BIGINT, score REAL, active BOOLEAN DEFAULT true, created TIMESTAMP DEFAULT CURRENT_TIMESTAMP, born DATE, token CHAR(36), meta JSON)"
        );
    }

    #[test]
    fn serial_without_primary_key_is_a_unique_auto_increment() {
        assert_eq!(
            run_sql("CREATE TABLE t (n SERIAL, label TEXT)"),
            "CREATE TABLE t (n BIGINT NOT NULL AUTO_INCREMENT UNIQUE, label TEXT)"
        );
    }

    #[test]
    fn a_text_primary_key_becomes_varchar() {
        assert_eq!(
            run_sql("CREATE TABLE IF NOT EXISTS docs (slug TEXT PRIMARY KEY, body TEXT)"),
            "CREATE TABLE IF NOT EXISTS docs (slug VARCHAR(255) PRIMARY KEY, body TEXT)"
        );
    }

    #[test]
    fn text_and_json_defaults_become_expressions() {
        assert_eq!(
            run_sql("CREATE TABLE t (status TEXT DEFAULT 'draft', tags JSON DEFAULT '[]', code TEXT UNIQUE DEFAULT 'x', note TEXT DEFAULT NULL)"),
            "CREATE TABLE t (status TEXT DEFAULT ('draft'), tags JSON DEFAULT ('[]'), code VARCHAR(255) UNIQUE DEFAULT 'x', note TEXT DEFAULT NULL)"
        );
    }

    #[test]
    fn inline_references_become_table_foreign_keys() {
        assert_eq!(
            run_sql("CREATE TABLE IF NOT EXISTS posts (id SERIAL PRIMARY KEY, author_id INTEGER REFERENCES users(id) ON DELETE CASCADE NOT NULL, editor_id INTEGER REFERENCES users(id) ON DELETE SET NULL, title TEXT)"),
            "CREATE TABLE IF NOT EXISTS posts (id BIGINT AUTO_INCREMENT PRIMARY KEY, author_id BIGINT NOT NULL, editor_id BIGINT, title TEXT, FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE CASCADE, FOREIGN KEY (editor_id) REFERENCES users(id) ON DELETE SET NULL)"
        );
    }

    #[test]
    fn mysql_column_types_and_table_constraints_stay() {
        let sql = "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, price DECIMAL(10, 2), CONSTRAINT fk_u FOREIGN KEY (id) REFERENCES u(id), UNIQUE KEY (name)) ENGINE = InnoDB";
        assert_eq!(run_sql(sql), sql);
    }

    #[test]
    fn alter_table_add_column_translates_the_definition() {
        assert_eq!(
            run_sql("ALTER TABLE gadgets ADD COLUMN color TEXT DEFAULT 'red'"),
            "ALTER TABLE gadgets ADD COLUMN color TEXT DEFAULT ('red')"
        );
        assert_eq!(
            run_sql("ALTER TABLE posts ADD COLUMN owner_id INTEGER REFERENCES users(id)"),
            "ALTER TABLE posts ADD COLUMN owner_id BIGINT, ADD FOREIGN KEY (owner_id) REFERENCES users(id)"
        );
        for sql in [
            "ALTER TABLE gadgets RENAME COLUMN size TO weight",
            "ALTER TABLE gadgets DROP COLUMN temp",
            "ALTER TABLE t ADD CONSTRAINT fk_t_u FOREIGN KEY (u) REFERENCES u (id) ON DELETE CASCADE",
            "ALTER TABLE t DROP CONSTRAINT fk_t_u",
        ] {
            assert_eq!(run_sql(sql), sql);
        }
    }

    #[test]
    fn create_index_if_not_exists_is_parsed() {
        let s = one(
            "CREATE UNIQUE INDEX IF NOT EXISTS uniq_users_email ON users (email)",
            0,
        );
        let Action::CreateIndex(spec) = s.action else {
            panic!("expected CreateIndex");
        };
        assert!(spec.unique);
        assert!(spec.if_not_exists);
        assert_eq!(spec.name, "uniq_users_email");
        assert_eq!(spec.table.name, "users");
        let text: HashSet<String> = ["email".to_string()].into_iter().collect();
        assert_eq!(
            spec.render(&text),
            "CREATE UNIQUE INDEX uniq_users_email ON users (email(255))"
        );
        assert_eq!(
            spec.render(&HashSet::new()),
            "CREATE UNIQUE INDEX uniq_users_email ON users (email)"
        );
    }

    #[test]
    fn create_index_keeps_order_and_expressions() {
        let s = one("CREATE INDEX idx_t ON t (a DESC, b, (LOWER(c)), d(10))", 0);
        let Action::CreateIndex(spec) = s.action else {
            panic!("expected CreateIndex");
        };
        assert!(!spec.if_not_exists);
        let text: HashSet<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            spec.render(&text),
            "CREATE INDEX idx_t ON t (a(255) DESC, b(255), (LOWER(c)), d(10))"
        );
    }

    #[test]
    fn drop_index_without_a_table() {
        let s = one("DROP INDEX uniq_gadgets_color", 0);
        assert_eq!(
            s.action,
            Action::DropIndex {
                name: "uniq_gadgets_color".into(),
                table: None,
                if_exists: false
            }
        );
        let s = one("DROP INDEX IF EXISTS idx_a ON gadgets", 0);
        let Action::DropIndex {
            name,
            table,
            if_exists,
        } = s.action
        else {
            panic!("expected DropIndex");
        };
        assert_eq!(name, "idx_a");
        assert_eq!(table.map(|t| t.name), Some("gadgets".to_string()));
        assert!(if_exists);
    }

    #[test]
    fn migration_with_several_statements() {
        let s = translate(
            "CREATE TABLE gadgets (id SERIAL PRIMARY KEY, name TEXT NOT NULL, size INTEGER); CREATE INDEX idx_gadgets_name_size ON gadgets (name, size); ALTER TABLE gadgets ADD COLUMN color TEXT; ALTER TABLE gadgets RENAME COLUMN size TO weight; INSERT INTO gadgets (name, weight, color) VALUES ('first', 3, 'red')",
            0,
        )
        .unwrap();
        assert_eq!(s.len(), 5);
        assert!(matches!(s[1].action, Action::CreateIndex(_)));
        assert_eq!(
            s[4].sql,
            "INSERT INTO gadgets (name, weight, color) VALUES ('first', 3, 'red')"
        );
    }

    #[test]
    fn comments_and_strings_with_semicolons_do_not_split() {
        let s = translate("SELECT 'a;b' /* ; */ FROM t # ;\n", 0).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].sql, "SELECT 'a;b' /* ; */ FROM t # ;");
    }
}
