use anyhow::{Context, Result};
use rusqlite::{
    params, Connection, OptionalExtension, Row, ToSql, Transaction, TransactionBehavior,
};
use std::collections::HashSet;
use std::path::Path;

/// Markers placed around matched terms in `Match::lines`: Unicode
/// noncharacters, reserved for internal use, so unlike control characters or the
/// Private Use Area (icon fonts) they don't turn up in archived text.
pub const MATCH_START: char = '\u{FDD0}';
pub const MATCH_END: char = '\u{FDD1}';
/// Prefix of the lines that stand for tool calls in stored messages.
pub const TOOL_USE_PREFIX: &str = "[tool_use: ";

/// Convert project directory names like "-home-murou-ghq-github-com-user-repo" to "user/repo"
pub fn format_project_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.trim_start_matches('-').split('-').collect();

    // Try to find github-com pattern and extract user/repo
    for (i, part) in parts.iter().enumerate() {
        if *part == "github" && parts.get(i + 1) == Some(&"com") && i + 3 < parts.len() {
            let user = parts[i + 2];
            let repo = parts[i + 3..].join("-");
            if let Some(pos) = repo.find("--worktrees") {
                return format!("{user}/{}", &repo[..pos]);
            }
            return format!("{user}/{repo}");
        }
    }

    // Fallback: take last meaningful segments
    let meaningful: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if meaningful.len() > 2 {
        meaningful[meaningful.len() - 2..].join("/")
    } else {
        raw.to_string()
    }
}

pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open database: {}", path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )?;

    init_schema(&conn)?;

    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            project    TEXT NOT NULL,
            started_at TEXT,
            imported_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS messages (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            uuid       TEXT,
            role       TEXT NOT NULL,
            content    TEXT NOT NULL,
            timestamp  TEXT,
            agent_id   TEXT,
            is_meta    INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY (session_id) REFERENCES sessions(session_id)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_uuid
            ON messages(uuid) WHERE uuid IS NOT NULL;

        CREATE INDEX IF NOT EXISTS idx_messages_session
            ON messages(session_id, id);
        ",
    )?;
    migrate_message_columns(conn)?;
    // Finds the question each search match belongs to, and where its exchange
    // ends (`search_exchanges`, `get_exchange`), without scanning the session.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_questions
            ON messages(session_id, agent_id, id) WHERE role = 'user' AND is_meta = 0",
    )?;

    // Create FTS table with Porter stemming, or migrate from old schema
    migrate_fts(conn)?;

    // Older databases re-index on every update; `set_message_origin` only touches
    // other columns, so recreate the trigger to fire on content changes alone.
    let update_trigger: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_au'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if update_trigger.is_some_and(|sql| !sql.contains("UPDATE OF content")) {
        conn.execute_batch("DROP TRIGGER IF EXISTS messages_au")?;
    }

    conn.execute_batch(
        "
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE OF content ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;
        ",
    )?;
    Ok(())
}

/// Columns added to `messages` after the first release, with the SQL adding each.
const ADDED_MESSAGE_COLUMNS: [(&str, &str); 2] = [
    ("agent_id", "ALTER TABLE messages ADD COLUMN agent_id TEXT"),
    (
        "is_meta",
        "ALTER TABLE messages ADD COLUMN is_meta INTEGER NOT NULL DEFAULT 0",
    ),
];

/// Add the `messages` columns that databases created by older versions lack.
fn migrate_message_columns(conn: &Connection) -> Result<()> {
    if missing_message_columns(conn)?.is_empty() {
        return Ok(());
    }
    // Hooks may open the database from several processes at once: take the write
    // lock before checking again, so only one of them adds the columns.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    for (column, sql) in missing_message_columns(&tx)? {
        tx.execute_batch(sql)?;
        if column == "is_meta" {
            // Transcripts that are gone can't be imported again to fill this in;
            // recognize what we can from the stored text.
            let [interrupted, continued] = META_TEXT_PREFIXES;
            tx.execute(
                "UPDATE messages SET is_meta = 1
                 WHERE role = 'user'
                   AND (substr(ltrim(content, char(32, 9, 10, 13)), 1, length(?1)) = ?1
                        OR substr(ltrim(content, char(32, 9, 10, 13)), 1, length(?2)) = ?2)",
                params![interrupted, continued],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn missing_message_columns(conn: &Connection) -> Result<Vec<(&'static str, &'static str)>> {
    let columns: HashSet<String> = conn
        .prepare("SELECT name FROM pragma_table_info('messages')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(ADDED_MESSAGE_COLUMNS
        .into_iter()
        .filter(|(column, _)| !columns.contains(*column))
        .collect())
}

/// Ensure the FTS table uses Porter stemming tokenizer.
/// Migrates from the old tokenizer if needed.
fn migrate_fts(conn: &Connection) -> Result<()> {
    let fts_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')",
        [],
        |row| row.get(0),
    )?;

    if fts_exists {
        // Check if the FTS table already uses porter tokenizer
        let create_sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='messages_fts'",
            [],
            |row| row.get(0),
        )?;

        if create_sql.contains("porter") {
            return Ok(());
        }

        // Old schema — drop and recreate with porter tokenizer
        eprintln!("Migrating FTS index to Porter stemming tokenizer...");
        conn.execute_batch(
            "
            DROP TRIGGER IF EXISTS messages_ai;
            DROP TRIGGER IF EXISTS messages_ad;
            DROP TRIGGER IF EXISTS messages_au;
            DROP TABLE messages_fts;
            ",
        )?;
    }

    conn.execute_batch(
        "
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            content,
            content_rowid='id',
            content='messages',
            tokenize='porter unicode61'
        );
        ",
    )?;

    // If migrating from old schema, rebuild the index from existing messages
    if fts_exists {
        conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild')")?;
        eprintln!("FTS index rebuilt with Porter stemming.");
    }

    Ok(())
}

pub fn upsert_session(
    conn: &Connection,
    session_id: &str,
    project: &str,
    started_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_id, project, started_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET
            project = excluded.project,
            started_at = COALESCE(
                MIN(excluded.started_at, sessions.started_at),
                excluded.started_at,
                sessions.started_at
            )",
        params![session_id, project, started_at],
    )?;
    Ok(())
}

/// Archive a message, unless one with the same uuid is archived already; returns
/// whether it was inserted. For a message already archived, a non-default
/// `origin` is still recorded, filling it in for messages archived before
/// origins were tracked.
pub fn insert_message(
    conn: &Connection,
    session_id: &str,
    uuid: Option<&str>,
    role: &str,
    content: &str,
    timestamp: Option<&str>,
    origin: &MessageOrigin,
) -> Result<bool> {
    // Skip if uuid already exists
    if let Some(uuid_val) = uuid {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE uuid = ?1)",
            params![uuid_val],
            |row| row.get(0),
        )?;
        if exists {
            if *origin != MessageOrigin::default() {
                set_message_origin(conn, uuid_val, origin)?;
            }
            return Ok(false);
        }
    }

    conn.execute(
        "INSERT INTO messages (session_id, uuid, role, content, timestamp, agent_id, is_meta)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session_id,
            uuid,
            role,
            content,
            timestamp,
            origin.agent_id,
            origin.is_meta
        ],
    )?;
    Ok(true)
}

/// How user messages that Claude Code wrote begin: interrupt markers and
/// compaction summaries.
const META_TEXT_PREFIXES: [&str; 2] = [
    "[Request interrupted by user",
    "This session is being continued from a previous conversation",
];

/// Whether a message's text shows Claude Code wrote it, for transcripts without
/// flags saying so.
pub fn is_meta_text(role: &str, content: &str) -> bool {
    role == "user"
        && META_TEXT_PREFIXES
            .iter()
            // The characters the migration in `migrate_message_columns` trims.
            .any(|prefix| {
                content
                    .trim_start_matches([' ', '\t', '\n', '\r'])
                    .starts_with(prefix)
            })
}

/// Where a message came from, beyond its role.
#[derive(Debug, Default, PartialEq)]
pub struct MessageOrigin<'a> {
    /// The subagent that wrote it; `None` for the main conversation.
    pub agent_id: Option<&'a str>,
    /// Added by Claude Code rather than typed by the user (meta messages,
    /// compaction summaries, interrupt markers), so it doesn't start a new question.
    pub is_meta: bool,
}

/// Record the origin of the archived message with this uuid.
pub fn set_message_origin(conn: &Connection, uuid: &str, origin: &MessageOrigin) -> Result<()> {
    conn.execute(
        "UPDATE messages SET agent_id = ?2, is_meta = ?3
         WHERE uuid = ?1 AND (agent_id IS NOT ?2 OR is_meta IS NOT ?3)",
        params![uuid, origin.agent_id, origin.is_meta],
    )?;
    Ok(())
}

pub struct SearchResult {
    pub session_id: String,
    pub project: String,
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

/// Escape a query string for safe use in FTS5 MATCH.
/// Wraps each token in double quotes to prevent FTS5 operator interpretation.
fn escape_fts_query(query: &str) -> String {
    // If the user already used explicit FTS5 syntax (AND, OR, NOT, quotes), pass through
    if query.contains('"')
        || query.contains(" AND ")
        || query.contains(" OR ")
        || query.contains(" NOT ")
    {
        return query.to_string();
    }
    // Otherwise, quote each whitespace-separated token
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The upper bound for an `--until` value, compared with `<=`. Timestamps are
/// ISO 8601 strings, so a value includes everything it's a prefix of: `2024`,
/// `2024-06`, `2024-06-30` and `2024-06-30T12:00` cover the whole year, month,
/// day and minute. Hence the value followed by the highest character.
fn until_bound(until: &str) -> String {
    format!("{until}{}", char::MAX)
}

/// When a session (aliased `s`) started or, without timestamps, was imported.
/// `imported_at` is in SQLite's "YYYY-MM-DD HH:MM:SS" form, which would compare
/// as text before ISO 8601 times on the same day ("T" sorts after the space),
/// so it's converted to theirs.
const SESSION_START: &str = "COALESCE(s.started_at, strftime('%Y-%m-%dT%H:%M:%SZ', s.imported_at))";

/// Normalize a project filter so that "user/repo" also matches raw dir names like
/// "-home-foo-ghq-github-com-user-repo". Replaces "/" with "-" for LIKE matching.
fn normalize_project_filter(filter: &str) -> String {
    let normalized = filter.replace('/', "-");
    format!("%{normalized}%")
}

/// Map a `session_id, project, role, content, timestamp` row.
fn search_result_from_row(row: &Row) -> rusqlite::Result<SearchResult> {
    Ok(SearchResult {
        session_id: row.get(0)?,
        project: row.get(1)?,
        role: row.get(2)?,
        content: row.get(3)?,
        timestamp: row.get(4)?,
    })
}

/// Which projects to look in, by their stored names: the names of the
/// directories in `~/.claude/projects`, or as given to `import-file`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProjectFilter<'a> {
    /// Names containing this; "user/repo" also matches raw directory names.
    Containing(&'a str),
    /// Exactly this name.
    Exact(&'a str),
}

/// The condition `filter` puts on `s.project`, binding its value as the next
/// parameter in `params_vec`.
fn project_condition(filter: ProjectFilter, params_vec: &mut Vec<Box<dyn ToSql>>) -> String {
    match filter {
        ProjectFilter::Containing(name) => {
            params_vec.push(Box::new(normalize_project_filter(name)));
            format!("s.project LIKE ?{}", params_vec.len())
        }
        ProjectFilter::Exact(name) => {
            params_vec.push(Box::new(name.to_string()));
            format!("s.project = ?{}", params_vec.len())
        }
    }
}

/// Optional restrictions on which messages a full-text search matches.
#[derive(Default)]
pub struct SearchFilters<'a> {
    pub project: Option<ProjectFilter<'a>>,
    /// Only this session's messages.
    pub session: Option<&'a str>,
    pub role: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
}

/// Build the `FROM ... WHERE ...` clause shared by full-text searches over messages
/// (aliased `m`) and their sessions (`s`). The FTS query is bound to `?1`; filter
/// values follow in order.
fn fts_from_where(escaped_query: String, filters: &SearchFilters) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = String::from(
        " FROM messages_fts f
         JOIN messages m ON m.id = f.rowid
         JOIN sessions s ON s.session_id = m.session_id
         WHERE messages_fts MATCH ?1",
    );
    let mut params_vec: Vec<Box<dyn ToSql>> = vec![Box::new(escaped_query)];

    if let Some(project) = filters.project {
        let condition = project_condition(project, &mut params_vec);
        sql.push_str(&format!(" AND {condition}"));
    }
    if let Some(session) = filters.session {
        params_vec.push(Box::new(session.to_string()));
        sql.push_str(&format!(" AND m.session_id = ?{}", params_vec.len()));
    }
    if let Some(role) = filters.role {
        params_vec.push(Box::new(role.to_string()));
        sql.push_str(&format!(" AND m.role = ?{}", params_vec.len()));
    }
    if let Some(s) = filters.since {
        params_vec.push(Box::new(s.to_string()));
        sql.push_str(&format!(" AND m.timestamp >= ?{}", params_vec.len()));
    }
    if let Some(u) = filters.until {
        params_vec.push(Box::new(until_bound(u)));
        sql.push_str(&format!(" AND m.timestamp <= ?{}", params_vec.len()));
    }
    (sql, params_vec)
}

pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    project_filter: Option<&str>,
    role_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let escaped = escape_fts_query(query);
    if escaped.is_empty() {
        return Ok(vec![]);
    }

    let filters = SearchFilters {
        project: project_filter.map(ProjectFilter::Containing),
        role: role_filter,
        since,
        until,
        ..Default::default()
    };
    let (from_where, mut params_vec) = fts_from_where(escaped, &filters);
    params_vec.push(Box::new(limit as i64));
    let sql = format!(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp{from_where}
         ORDER BY rank LIMIT ?{}",
        params_vec.len()
    );

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    let results = stmt
        .query_map(param_refs.as_slice(), search_result_from_row)?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(results)
}

/// A question/answer exchange: a question (a user message that isn't meta) plus
/// everything up to the next question in the same thread, which is either the
/// main conversation or one subagent.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub session_id: String,
    pub project: String,
    /// When the session started (or was imported, without timestamps).
    pub session_started: Option<String>,
    /// The subagent whose thread the exchange belongs to; `None` for the main conversation.
    pub agent_id: Option<String>,
    /// The question starting the exchange; `None` for the messages that come
    /// before any question in their thread.
    pub question_id: Option<i64>,
    /// The start of the question (up to `PREVIEW_LEN` characters).
    pub question_preview: Option<String>,
    /// When the question was asked (or, without a question, when the exchange's
    /// first message, or the match, was written).
    pub timestamp: Option<String>,
    /// Where a search matched the exchange; `None` when not found by a search.
    pub matched: Option<Match>,
}

/// The best-ranked match of a search in an exchange.
#[derive(Clone, Debug)]
pub struct Match {
    /// Role of the matching message.
    pub role: String,
    /// The message's line with the match and the lines around it (up to
    /// `CONTEXT_LINES`, blank ones skipped), with matched terms between
    /// `MATCH_START` and `MATCH_END`.
    pub lines: Vec<String>,
}

/// Lines in `Match::lines`.
pub const CONTEXT_LINES: usize = 3;
/// Characters of the matching line kept before the match.
const SNIPPET_LEAD: usize = 30;
/// Maximum characters of a line in `Match::lines`, besides ellipses.
const SNIPPET_LEN: usize = 300;
/// Maximum characters of a question kept for previews.
const PREVIEW_LEN: usize = 400;

/// Full-text search returning up to `limit` distinct exchanges in rank order,
/// each represented by its best-matching message. Meta messages don't count as
/// matches, and unless `include_tools` is set, neither do matches that are only
/// in `[tool_use: ...]` lines.
pub fn search_exchanges(
    conn: &Connection,
    query: &str,
    limit: usize,
    filters: &SearchFilters,
    include_tools: bool,
) -> Result<Vec<Exchange>> {
    let escaped = escape_fts_query(query);
    if escaped.is_empty() || limit == 0 {
        return Ok(vec![]);
    }

    let (from_where, params_vec) = fts_from_where(escaped, filters);
    let sql = format!(
        "SELECT m.session_id, s.project, m.role, m.timestamp, m.agent_id,
                (SELECT u.id FROM messages u
                 WHERE u.session_id = m.session_id AND u.agent_id IS m.agent_id
                   AND u.role = 'user' AND u.is_meta = 0 AND u.id <= m.id
                 ORDER BY u.id DESC LIMIT 1),
                highlight(messages_fts, 0, char({}), char({})),
                {SESSION_START}{from_where}
           AND m.is_meta = 0
         ORDER BY rank",
        MATCH_START as u32, MATCH_END as u32,
    );

    let mut hits = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let mut rows = stmt.query(param_refs.as_slice())?;
        let mut seen = HashSet::new();
        // No LIMIT in SQL: several matches can fall in one exchange, and some are
        // skipped, so read rows until `limit` exchanges have been found.
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let agent_id: Option<String> = row.get(4)?;
            let question_id: Option<i64> = row.get(5)?;
            let exchange = (session_id, agent_id, question_id);
            if seen.contains(&exchange) {
                continue;
            }
            let marked = row.get_ref(6)?.as_str()?;
            let lines = if include_tools {
                context_around_match(marked)
            } else {
                context_around_match(&strip_marked_tool_lines(marked))
            };
            // The match is only in tool calls, which the answer won't show.
            let Some(lines) = lines else { continue };
            let (session_id, agent_id, question_id) = exchange.clone();
            seen.insert(exchange);
            hits.push(Exchange {
                session_id,
                project: row.get(1)?,
                session_started: row.get(7)?,
                agent_id,
                question_id,
                question_preview: None,
                timestamp: row.get(3)?,
                matched: Some(Match {
                    role: row.get(2)?,
                    lines,
                }),
            });
            if hits.len() == limit {
                break;
            }
        }
    }

    let mut question_stmt = conn.prepare(&format!(
        "SELECT substr(content, 1, {PREVIEW_LEN}), timestamp FROM messages WHERE id = ?1"
    ))?;
    for hit in &mut hits {
        if let Some(question_id) = hit.question_id {
            let question: Option<(String, Option<String>)> = question_stmt
                .query_row(params![question_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            if let Some((preview, timestamp)) = question {
                hit.question_preview = Some(preview);
                hit.timestamp = timestamp.or(hit.timestamp.take());
            }
        }
    }

    Ok(hits)
}

/// Drop `[tool_use: ...]` lines from `highlight()` output, as `strip_tool_lines`
/// does for plain content, ignoring match markers inside the prefix.
fn strip_marked_tool_lines(marked: &str) -> String {
    marked
        .lines()
        .filter(|line| {
            let plain: String = line
                .chars()
                .filter(|&c| c != MATCH_START && c != MATCH_END)
                .collect();
            !plain.starts_with(TOOL_USE_PREFIX)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The line with the first match in `marked` and those around it, centred on it
/// where there are enough, or `None` without a match. Blank lines are skipped,
/// and long lines cut: the matching one so that the match stays in view.
fn context_around_match(marked: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = marked.lines().filter(|l| !l.trim().is_empty()).collect();
    let at = lines.iter().position(|l| l.contains(MATCH_START))?;
    let start = at
        .saturating_sub(CONTEXT_LINES / 2)
        .min(lines.len().saturating_sub(CONTEXT_LINES));
    let end = lines.len().min(start + CONTEXT_LINES);
    let context = (start..end)
        .map(|i| {
            let chars: Vec<char> = lines[i].trim().chars().collect();
            let from = match chars.iter().position(|&c| c == MATCH_START) {
                Some(hit) if i == at => hit.saturating_sub(SNIPPET_LEAD),
                _ => 0,
            };
            let to = chars.len().min(from + SNIPPET_LEN);
            let mut line = String::new();
            if from > 0 {
                line.push('…');
            }
            line.extend(&chars[from..to]);
            if to < chars.len() {
                line.push('…');
            }
            line
        })
        .collect();
    Some(context)
}

/// Messages of one exchange in order: the question (`question_id`) followed by
/// everything up to the next question in the same thread (`agent_id`). With
/// `question_id` = `None`, the messages before the thread's first question.
pub fn get_exchange(
    conn: &Connection,
    session_id: &str,
    agent_id: Option<&str>,
    question_id: Option<i64>,
) -> Result<Vec<SearchResult>> {
    let mut stmt = conn.prepare(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
         FROM messages m
         JOIN sessions s ON s.session_id = m.session_id
         WHERE m.session_id = ?1 AND m.agent_id IS ?2 AND m.id >= ?3
           AND m.id < COALESCE(
               (SELECT MIN(n.id) FROM messages n
                WHERE n.session_id = ?1 AND n.agent_id IS ?2
                  AND n.role = 'user' AND n.is_meta = 0 AND n.id > ?3),
               9223372036854775807)
         ORDER BY m.id ASC",
    )?;
    let results = stmt
        .query_map(
            params![session_id, agent_id, question_id.unwrap_or(0)],
            search_result_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(results)
}

/// Every exchange of a session, in no particular order: one per question, plus
/// one for each thread whose first answer comes before any question in it.
pub fn session_exchanges(conn: &Connection, session_id: &str) -> Result<Vec<Exchange>> {
    let session: Option<(String, Option<String>)> = conn
        .query_row(
            &format!("SELECT s.project, {SESSION_START} FROM sessions s WHERE s.session_id = ?1"),
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((project, session_started)) = session else {
        return Ok(Vec::new());
    };
    let exchange = |agent_id, question_id, question_preview, timestamp| Exchange {
        session_id: session_id.to_string(),
        project: project.clone(),
        session_started: session_started.clone(),
        agent_id,
        question_id,
        question_preview,
        timestamp,
        matched: None,
    };

    let mut questions = conn.prepare(&format!(
        "SELECT agent_id, id, substr(content, 1, {PREVIEW_LEN}), timestamp FROM messages
         WHERE session_id = ?1 AND role = 'user' AND is_meta = 0"
    ))?;
    let mut exchanges = questions
        .query_map(params![session_id], |row| {
            Ok(exchange(
                row.get(0)?,
                Some(row.get(1)?),
                Some(row.get(2)?),
                row.get(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // The timestamp is the first answer's: SQLite takes bare columns from the
    // row that MIN() picks.
    let mut first_answers = conn.prepare(
        "SELECT agent_id, MIN(id), timestamp FROM messages
         WHERE session_id = ?1 AND role = 'assistant' AND is_meta = 0
         GROUP BY agent_id",
    )?;
    let first_answers = first_answers
        .query_map(params![session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (agent_id, first_answer, timestamp) in first_answers {
        let asked_before = exchanges
            .iter()
            .any(|e| e.agent_id == agent_id && e.question_id.is_some_and(|id| id < first_answer));
        if !asked_before {
            exchanges.push(exchange(agent_id, None, None, timestamp));
        }
    }
    Ok(exchanges)
}

/// A session, as listed by interactive mode.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub project: String,
    /// When the session started (or was imported, without timestamps).
    pub started: Option<String>,
    /// Questions asked in the session, including subagents' prompts.
    pub questions: usize,
    /// The start of the first question in the main conversation.
    pub first_question: Option<String>,
}

/// The `WHERE` clause selecting the sessions of the projects `project` selects
/// (all without one) that started (or, without timestamps, were imported) from
/// `since` up to `until`, and its parameters.
fn sessions_where(
    project: Option<ProjectFilter>,
    since: Option<&str>,
    until: Option<&str>,
) -> (String, Vec<Box<dyn ToSql>>) {
    let mut params_vec: Vec<Box<dyn ToSql>> = Vec::new();
    let mut conditions = vec!["1".to_string()];
    if let Some(project) = project {
        conditions.push(project_condition(project, &mut params_vec));
    }
    if let Some(since) = since {
        params_vec.push(Box::new(since.to_string()));
        conditions.push(format!("{SESSION_START} >= ?{}", params_vec.len()));
    }
    if let Some(until) = until {
        params_vec.push(Box::new(until_bound(until)));
        conditions.push(format!("{SESSION_START} <= ?{}", params_vec.len()));
    }
    (format!("WHERE {}", conditions.join(" AND ")), params_vec)
}

/// The sessions of the projects `project` selects (all without one) that
/// started (or, without timestamps, were imported) from `since` up to `until`,
/// in no particular order.
pub fn project_sessions(
    conn: &Connection,
    project: Option<ProjectFilter>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionInfo>> {
    let (where_clause, params_vec) = sessions_where(project, since, until);
    let sql = format!(
        "SELECT s.session_id, s.project, {SESSION_START},
                (SELECT COUNT(*) FROM messages q
                 WHERE q.session_id = s.session_id AND q.role = 'user' AND q.is_meta = 0),
                (SELECT substr(q.content, 1, {PREVIEW_LEN}) FROM messages q
                 WHERE q.session_id = s.session_id AND q.agent_id IS NULL
                   AND q.role = 'user' AND q.is_meta = 0
                 ORDER BY q.id LIMIT 1)
         FROM sessions s
         {where_clause}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    let sessions = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionInfo {
                session_id: row.get(0)?,
                project: row.get(1)?,
                started: row.get(2)?,
                questions: row.get::<_, i64>(3)?.try_into().unwrap_or(0),
                first_question: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(sessions)
}

/// A project, as listed by interactive mode.
#[derive(Clone, Debug)]
pub struct ProjectInfo {
    pub project: String,
    pub sessions: usize,
    /// When its latest session started (or was imported, without timestamps).
    pub latest: Option<String>,
}

/// The projects `project` selects (all without one) with sessions started from
/// `since` up to `until`, as `project_sessions` counts them, in no particular
/// order.
pub fn projects(
    conn: &Connection,
    project: Option<ProjectFilter>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<ProjectInfo>> {
    let (where_clause, params_vec) = sessions_where(project, since, until);
    let sql = format!(
        "SELECT s.project, COUNT(*), MAX({SESSION_START})
         FROM sessions s
         {where_clause}
         GROUP BY s.project"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    let projects = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ProjectInfo {
                project: row.get(0)?,
                sessions: row.get::<_, i64>(1)?.try_into().unwrap_or(0),
                latest: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(projects)
}

/// Whether any session is archived under exactly this project name.
pub fn project_exists(conn: &Connection, project: &str) -> Result<bool> {
    let exists = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sessions WHERE project = ?1)",
        params![project],
        |row| row.get(0),
    )?;
    Ok(exists)
}

pub fn stats(conn: &Connection) -> Result<(i64, i64)> {
    let session_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    let message_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    Ok((session_count, message_count))
}

/// Resolve a session ID prefix to a full session ID.
/// Returns an error if the prefix matches zero or multiple sessions.
pub fn resolve_session_id(conn: &Connection, prefix: &str) -> Result<String> {
    let mut stmt =
        conn.prepare("SELECT session_id FROM sessions WHERE session_id LIKE ?1 || '%'")?;
    let matches: Vec<String> = stmt
        .query_map(params![prefix], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    match matches.len() {
        0 => anyhow::bail!("No session found matching: {prefix}"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let previews: Vec<String> = matches.iter().take(5).cloned().collect();
            anyhow::bail!(
                "Ambiguous prefix '{prefix}' matches {n} sessions:\n  {}",
                previews.join("\n  ")
            );
        }
    }
}

pub fn get_session_messages(conn: &Connection, session_id: &str) -> Result<Vec<SearchResult>> {
    let mut stmt = conn.prepare(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
         FROM messages m
         JOIN sessions s ON s.session_id = m.session_id
         WHERE m.session_id = ?1
         ORDER BY m.id ASC",
    )?;

    let results = stmt
        .query_map(params![session_id], search_result_from_row)?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(results)
}

/// Get the Nth most recent session ID (0-indexed).
pub fn nth_recent_session_id(conn: &Connection, n: usize) -> Result<String> {
    conn.query_row(
        &format!(
            "SELECT s.session_id FROM sessions s
             ORDER BY {SESSION_START} DESC
             LIMIT 1 OFFSET ?1"
        ),
        params![n as i64],
        |row| row.get(0),
    )
    .with_context(|| format!("No session found at position {}", n + 1))
}

pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    pub started_at: Option<String>,
    pub message_count: i64,
    pub first_user_message: Option<String>,
}

pub fn list_sessions(
    conn: &Connection,
    limit: usize,
    project_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    let mut sql = String::from(
        "SELECT s.session_id, s.project, s.started_at,
                (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.session_id) as msg_count,
                NULL as first_msg
         FROM sessions s",
    );
    let mut conditions = Vec::new();
    let mut param_idx = 1;
    if project_filter.is_some() {
        conditions.push(format!("s.project LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if since.is_some() {
        conditions.push(format!("{SESSION_START} >= ?{param_idx}"));
        param_idx += 1;
    }
    if until.is_some() {
        conditions.push(format!("{SESSION_START} <= ?{param_idx}"));
        param_idx += 1;
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(&format!(
        " ORDER BY {SESSION_START} DESC LIMIT ?{param_idx}"
    ));

    let mut stmt = conn.prepare(&sql)?;

    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(proj) = project_filter {
        params_vec.push(Box::new(normalize_project_filter(proj)));
    }
    if let Some(s) = since {
        params_vec.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        params_vec.push(Box::new(until_bound(u)));
    }
    params_vec.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let mut sessions: Vec<SessionSummary> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                project: row.get(1)?,
                started_at: row.get(2)?,
                message_count: row.get(3)?,
                first_user_message: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Fetch first meaningful user message for each session
    let mut user_stmt = conn.prepare(
        "SELECT substr(content, 1, 200) FROM messages
         WHERE session_id = ?1 AND role = 'user'
         ORDER BY id ASC LIMIT 10",
    )?;
    // Fallback: first assistant message
    let mut asst_stmt = conn.prepare(
        "SELECT substr(content, 1, 200) FROM messages
         WHERE session_id = ?1 AND role = 'assistant'
         ORDER BY id ASC LIMIT 3",
    )?;

    for session in &mut sessions {
        let candidates: Vec<String> = user_stmt
            .query_map(params![&session.session_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        session.first_user_message = candidates.into_iter().find(|c| is_meaningful_preview(c));

        // If no meaningful user message found, try assistant messages
        if session.first_user_message.is_none() {
            let asst_candidates: Vec<String> = asst_stmt
                .query_map(params![&session.session_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            session.first_user_message = asst_candidates
                .into_iter()
                .find(|c| is_meaningful_preview(c));
        }
    }

    Ok(sessions)
}

/// Check if a message is suitable as a session preview in `recent`.
/// Skips tool_result artifacts and system meta-messages.
fn is_meaningful_preview(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.len() < 5 {
        return false;
    }
    // Skip messages starting with JSON/XML/path characters (likely tool output)
    let first_char = trimmed.chars().next().unwrap_or(' ');
    if matches!(first_char, '{' | '[' | '<') {
        return false;
    }
    // System/meta messages that are never human input
    let noise_prefixes = [
        "Tool loaded",
        "This session is being continued",
        "Your task is to create a detailed summary",
    ];
    for prefix in &noise_prefixes {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }
    true
}

pub fn delete_session(conn: &Connection, session_id: &str) -> Result<u64> {
    let msg_deleted = conn.execute(
        "DELETE FROM messages WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM sessions WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(msg_deleted as u64)
}

pub fn verify(conn: &Connection) -> Result<()> {
    println!("=== 1. Messages by role ===");
    let mut stmt = conn.prepare("SELECT role, COUNT(*) FROM messages GROUP BY role")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (role, count) = row?;
        println!("  {role}: {count}");
    }

    println!("\n=== 2. Sessions and projects ===");
    let session_count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let project_count: i64 =
        conn.query_row("SELECT COUNT(DISTINCT project) FROM sessions", [], |r| {
            r.get(0)
        })?;
    println!("  Sessions: {session_count}, Projects: {project_count}");

    println!("\n=== 3. Top 5 projects by message count ===");
    let mut stmt = conn.prepare(
        "SELECT s.project, COUNT(*) as cnt FROM messages m
         JOIN sessions s ON s.session_id = m.session_id
         GROUP BY s.project ORDER BY cnt DESC LIMIT 5",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (proj, count) = row?;
        println!("  {}: {count}", format_project_name(&proj));
    }

    println!("\n=== 4. FTS index integrity ===");
    let msg_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let fts_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0))?;
    let fts_ok = msg_count == fts_count;
    println!("  messages: {msg_count} rows");
    println!("  messages_fts: {fts_count} rows");
    println!("  Match: {}", if fts_ok { "OK" } else { "FAIL" });

    println!("\n=== 5. UUID deduplication ===");
    let dup_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (SELECT uuid, COUNT(*) as cnt FROM messages WHERE uuid IS NOT NULL GROUP BY uuid HAVING cnt > 1)",
        [],
        |r| r.get(0),
    )?;
    let dup_ok = dup_count == 0;
    println!(
        "  Duplicate UUIDs: {dup_count} {}",
        if dup_ok { "OK" } else { "FAIL" }
    );

    println!("\n=== 6. Recent message samples ===");
    let mut stmt = conn.prepare(
        "SELECT m.role, substr(m.content, 1, 120), m.timestamp
         FROM messages m JOIN sessions s ON s.session_id = m.session_id
         ORDER BY m.timestamp DESC LIMIT 10",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (role, content, ts) = row?;
        let content = content.replace('\n', " ");
        println!("  [{role}] {} | {content}", ts.as_deref().unwrap_or("?"));
    }

    println!("\n=== 7. FTS5 search test ===");
    for query in ["import", "SQLite", "cargo"] {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            params![query],
            |r| r.get(0),
        )?;
        println!("  \"{query}\": {count} hits");
    }

    if !fts_ok || !dup_ok {
        anyhow::bail!("Verification failed");
    }
    println!("\nAll checks passed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn setup_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = open_db(tmp.path()).unwrap();
        (conn, tmp)
    }

    #[test]
    fn test_open_and_init() {
        let (_conn, _tmp) = setup_db();
    }

    #[test]
    fn test_upsert_session() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "my-project", Some("2024-01-01T00:00:00Z")).unwrap();
        upsert_session(&conn, "sess-1", "my-project", None).unwrap();
        // A subagent's transcript, imported later, starts after its session.
        upsert_session(&conn, "sess-1", "my-project", Some("2024-01-01T05:00:00Z")).unwrap();

        let (project, started_at): (String, String) = conn
            .query_row(
                "SELECT project, started_at FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(project, "my-project");
        assert_eq!(started_at, "2024-01-01T00:00:00Z");

        // An earlier start found later (e.g. the main transcript after a subagent's) wins.
        upsert_session(&conn, "sess-2", "p", Some("2024-01-01T05:00:00Z")).unwrap();
        upsert_session(&conn, "sess-2", "p", Some("2024-01-01T00:00:00Z")).unwrap();
        let started_at: String = conn
            .query_row(
                "SELECT started_at FROM sessions WHERE session_id = 'sess-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started_at, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn test_insert_and_search() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "proj", None).unwrap();
        insert_message(
            &conn,
            "sess-1",
            Some("u1"),
            "user",
            "hello world",
            Some("2024-01-01T00:00:00Z"),
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "sess-1",
            Some("u2"),
            "assistant",
            "hi there",
            Some("2024-01-01T00:00:01Z"),
            &MessageOrigin::default(),
        )
        .unwrap();

        let results = search(&conn, "hello", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "user");
        assert_eq!(results[0].content, "hello world");
    }

    #[test]
    fn test_porter_stemming() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "the server is running fine",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "user",
            "configure the database settings",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        // "run" should match "running" via stemming
        let results = search(&conn, "run", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("running"));

        // "configuration" should match "configure" via stemming
        let results = search(&conn, "configuration", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("configure"));
    }

    #[test]
    fn test_duplicate_uuid_skipped() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "proj", None).unwrap();
        let inserted = insert_message(
            &conn,
            "sess-1",
            Some("u1"),
            "user",
            "first",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        assert!(inserted);
        let inserted = insert_message(
            &conn,
            "sess-1",
            Some("u1"),
            "user",
            "duplicate",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        assert!(!inserted);

        let (_, msg_count) = stats(&conn).unwrap();
        assert_eq!(msg_count, 1);
    }

    #[test]
    fn test_stats() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "p1", None).unwrap();
        upsert_session(&conn, "s2", "p2", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "msg1",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "assistant",
            "msg2",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s2",
            Some("u3"),
            "user",
            "msg3",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let (sessions, messages) = stats(&conn).unwrap();
        assert_eq!(sessions, 2);
        assert_eq!(messages, 3);
    }

    #[test]
    fn test_search_no_results() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "hello",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let results = search(&conn, "nonexistent", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_with_date_filter() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "early message",
            Some("2024-01-01T00:00:00Z"),
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "user",
            "late message",
            Some("2024-06-01T00:00:00Z"),
            &MessageOrigin::default(),
        )
        .unwrap();

        let results = search(&conn, "message", 10, None, None, Some("2024-03-01"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("late"));

        let results = search(&conn, "message", 10, None, None, None, Some("2024-03-01")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("early"));
    }

    #[test]
    fn test_search_empty_query() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "hello",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let results = search(&conn, "", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());

        let results = search(&conn, "   ", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_project_filter_with_slash() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "-home-user-ghq-github-com-owner-repo", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "test msg",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        // Filter with formatted name "owner/repo"
        let results = search(&conn, "test", 10, Some("owner/repo"), None, None, None).unwrap();
        assert_eq!(results.len(), 1);

        // Filter with just "repo"
        let results = search(&conn, "test", 10, Some("repo"), None, None, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_delete_session() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "msg1",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "assistant",
            "msg2",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let deleted = delete_session(&conn, "s1").unwrap();
        assert_eq!(deleted, 2);

        let (sessions, messages) = stats(&conn).unwrap();
        assert_eq!(sessions, 0);
        assert_eq!(messages, 0);
    }

    #[test]
    fn test_list_sessions_with_date_filter() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", Some("2024-01-15T00:00:00Z")).unwrap();
        upsert_session(&conn, "s2", "proj", Some("2024-06-15T00:00:00Z")).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "old session",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s2",
            Some("u2"),
            "user",
            "new session",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let sessions = list_sessions(&conn, 100, None, Some("2024-03-01"), None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s2");

        let sessions = list_sessions(&conn, 100, None, None, Some("2024-03-01")).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
    }

    #[test]
    fn test_until_includes_the_whole_day() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", Some("2024-06-30T23:59:00Z")).unwrap();
        upsert_session(&conn, "s2", "proj", Some("2024-07-01T00:00:00Z")).unwrap();
        // Sessions without started_at fall back to imported_at ("YYYY-MM-DD HH:MM:SS").
        upsert_session(&conn, "s3", "proj", None).unwrap();
        conn.execute(
            "UPDATE sessions SET imported_at = '2024-06-30 12:00:00' WHERE session_id = 's3'",
            [],
        )
        .unwrap();
        let rows = [
            (
                "s1",
                "u1",
                "deploy late on the last day",
                "2024-06-30T23:59:00Z",
            ),
            (
                "s2",
                "u2",
                "deploy just after midnight",
                "2024-07-01T00:00:00Z",
            ),
        ];
        for (session, uuid, content, ts) in rows {
            insert_message(
                &conn,
                session,
                Some(uuid),
                "user",
                content,
                Some(ts),
                &MessageOrigin::default(),
            )
            .unwrap();
        }
        insert_message(
            &conn,
            "s3",
            Some("u3"),
            "user",
            "deploy",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let results = search(&conn, "deploy", 10, None, None, None, Some("2024-06-30")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("last day"));
        let filters = SearchFilters {
            until: Some("2024-06-30"),
            ..Default::default()
        };
        assert_eq!(
            search_exchanges(&conn, "deploy", 10, &filters, false)
                .unwrap()
                .len(),
            1
        );

        let mut sessions: Vec<String> = list_sessions(&conn, 100, None, None, Some("2024-06-30"))
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        sessions.sort();
        assert_eq!(sessions, ["s1", "s3"]);

        // Partial dates and times cover everything they're a prefix of.
        let count = |until| {
            search(&conn, "deploy", 10, None, None, None, Some(until))
                .unwrap()
                .len()
        };
        assert_eq!(count("2024-06"), 1);
        assert_eq!(count("2024"), 2);
        assert_eq!(count("2024-06-30T23:59"), 1);
        assert_eq!(count("2024-06-30T23:58"), 0);
        assert_eq!(count("2024-06-30T12:00:00Z"), 0);
        assert_eq!(count("2024-07-01T00:00:00Z"), 2);
    }

    #[test]
    fn test_until_bound_sorts_after_everything_it_prefixes() {
        let bound = until_bound("2024-06");
        for covered in [
            "2024-06",
            "2024-06-01T00:00:00Z",
            "2024-06-30T23:59:59.999Z",
        ] {
            assert!(*covered <= *bound, "{covered}");
        }
        for later in ["2024-07", "2024-07-01T00:00:00Z", "2025"] {
            assert!(*later > *bound, "{later}");
        }
    }

    /// One session: an answer before any question, then two question/answer exchanges.
    /// Returns the ids of the two questions.
    fn setup_exchanges(conn: &Connection) -> (i64, i64) {
        upsert_session(conn, "s1", "proj", None).unwrap();
        let rows = [
            (
                "a0",
                "assistant",
                "resuming the docker work",
                "2024-01-01T00:00:00Z",
            ),
            (
                "q1",
                "user",
                "how do I fix the docker build?",
                "2024-01-01T00:01:00Z",
            ),
            (
                "a1",
                "assistant",
                "Let me check the docker logs.",
                "2024-01-01T00:02:00Z",
            ),
            (
                "a2",
                "assistant",
                "Add --no-cache to the docker build.",
                "2024-01-01T00:03:00Z",
            ),
            ("q2", "user", "thanks, now deploy", "2024-01-01T00:04:00Z"),
            (
                "a3",
                "assistant",
                "Deploying with docker compose.",
                "2024-01-01T00:05:00Z",
            ),
        ];
        let origin = MessageOrigin::default();
        for (uuid, role, content, ts) in rows {
            insert_message(conn, "s1", Some(uuid), role, content, Some(ts), &origin).unwrap();
        }
        let id = |uuid: &str| -> i64 {
            conn.query_row("SELECT id FROM messages WHERE uuid = ?1", [uuid], |r| {
                r.get(0)
            })
            .unwrap()
        };
        (id("q1"), id("q2"))
    }

    #[test]
    fn test_search_exchanges_groups_matches_by_question() {
        let (conn, _tmp) = setup_db();
        let (q1, q2) = setup_exchanges(&conn);
        let none = SearchFilters::default();

        let hits = search_exchanges(&conn, "docker", 10, &none, false).unwrap();
        // Five messages match, in three exchanges.
        assert_eq!(hits.len(), 3);

        let first = hits.iter().find(|h| h.question_id == Some(q1)).unwrap();
        assert_eq!(
            first.question_preview.as_deref(),
            Some("how do I fix the docker build?")
        );
        assert_eq!(first.timestamp.as_deref(), Some("2024-01-01T00:01:00Z"));
        let matched = first.matched.as_ref().unwrap();
        assert!(matched.lines[0].contains(&format!("{MATCH_START}docker{MATCH_END}")));

        let second = hits.iter().find(|h| h.question_id == Some(q2)).unwrap();
        assert_eq!(
            second.question_preview.as_deref(),
            Some("thanks, now deploy")
        );
        assert_eq!(second.matched.as_ref().unwrap().role, "assistant");
        assert_eq!(second.timestamp.as_deref(), Some("2024-01-01T00:04:00Z"));

        let orphan = hits.iter().find(|h| h.question_id.is_none()).unwrap();
        assert_eq!(orphan.question_preview, None);
        assert_eq!(orphan.timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn test_search_exchanges_limit_counts_exchanges() {
        let (conn, _tmp) = setup_db();
        setup_exchanges(&conn);
        let none = SearchFilters::default();

        let hits = search_exchanges(&conn, "docker", 2, &none, false).unwrap();
        assert_eq!(hits.len(), 2);
        assert_ne!(hits[0].question_id, hits[1].question_id);

        assert!(search_exchanges(&conn, "docker", 0, &none, false)
            .unwrap()
            .is_empty());
        assert!(search_exchanges(&conn, "  ", 10, &none, false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_search_exchanges_skips_matches_only_in_tool_lines() {
        let (conn, _tmp) = setup_db();
        let (_, q2) = setup_exchanges(&conn);
        let rows = [
            ("t1", "[tool_use: Bash] {\"command\":\"kubectl apply\"}"),
            (
                "t2",
                "Let me check.\n[tool_use: Bash] {\"command\":\"helm install\"}",
            ),
            // "use" also matches inside the "[tool_use: " prefix itself.
            ("t3", "Done.\n[tool_use: Bash] {\"command\":\"ls\"}"),
        ];
        for (uuid, content) in rows {
            insert_message(
                &conn,
                "s1",
                Some(uuid),
                "assistant",
                content,
                None,
                &MessageOrigin::default(),
            )
            .unwrap();
        }
        let none = SearchFilters::default();

        for query in ["kubectl", "helm", "use"] {
            assert!(
                search_exchanges(&conn, query, 10, &none, false)
                    .unwrap()
                    .is_empty(),
                "{query} matched a hidden tool line"
            );
        }
        let hits = search_exchanges(&conn, "helm", 10, &none, true).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].question_id, Some(q2));
        let matched = hits[0].matched.as_ref().unwrap();
        assert!(matched
            .lines
            .iter()
            .any(|line| line.contains(&format!("{MATCH_START}helm{MATCH_END}"))));
    }

    #[test]
    fn test_subagent_messages_form_their_own_thread() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        let subagent = MessageOrigin {
            agent_id: Some("agent-1"),
            is_meta: false,
        };
        // As imported by hooks mid-session: the subagent's file lands between
        // two parts of the main conversation.
        let rows = [
            ("q1", "user", "why does the deploy fail?", None),
            ("a1", "assistant", "Let me look into the deploy.", None),
            ("sq", "user", "Investigate the deploy logs", Some(&subagent)),
            (
                "sa",
                "assistant",
                "The deploy logs show a timeout.",
                Some(&subagent),
            ),
            ("a2", "assistant", "The deploy fails on a timeout.", None),
        ];
        for (uuid, role, content, origin) in rows {
            let origin = origin.unwrap_or(&MessageOrigin {
                agent_id: None,
                is_meta: false,
            });
            insert_message(&conn, "s1", Some(uuid), role, content, None, origin).unwrap();
        }
        let none = SearchFilters::default();

        let hits = search_exchanges(&conn, "timeout", 10, &none, false).unwrap();
        assert_eq!(hits.len(), 2);
        let main = hits.iter().find(|h| h.agent_id.is_none()).unwrap();
        assert_eq!(
            main.question_preview.as_deref(),
            Some("why does the deploy fail?")
        );
        let side = hits.iter().find(|h| h.agent_id.is_some()).unwrap();
        assert_eq!(side.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(
            side.question_preview.as_deref(),
            Some("Investigate the deploy logs")
        );

        let contents = |agent_id, question_id| -> Vec<String> {
            get_exchange(&conn, "s1", agent_id, question_id)
                .unwrap()
                .into_iter()
                .map(|m| m.content)
                .collect()
        };
        assert_eq!(
            contents(None, main.question_id),
            [
                "why does the deploy fail?",
                "Let me look into the deploy.",
                "The deploy fails on a timeout.",
            ]
        );
        assert_eq!(
            contents(Some("agent-1"), side.question_id),
            [
                "Investigate the deploy logs",
                "The deploy logs show a timeout."
            ]
        );
    }

    #[test]
    fn test_meta_messages_do_not_start_questions_or_match() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        let rows = [
            ("q1", "user", "move the schema", false),
            ("a1", "assistant", "Starting the migration.", false),
            (
                "m1",
                "user",
                "This session is being continued: migration in progress",
                true,
            ),
            ("a2", "assistant", "The migration is finished.", false),
        ];
        for (uuid, role, content, is_meta) in rows {
            let origin = MessageOrigin {
                agent_id: None,
                is_meta,
            };
            insert_message(&conn, "s1", Some(uuid), role, content, None, &origin).unwrap();
        }
        let none = SearchFilters::default();

        // The summary is neither a match nor the start of a new exchange.
        assert!(search_exchanges(&conn, "continued", 10, &none, false)
            .unwrap()
            .is_empty());
        let hits = search_exchanges(&conn, "migration", 10, &none, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].question_preview.as_deref(), Some("move the schema"));
        let exchange = get_exchange(&conn, "s1", None, hits[0].question_id).unwrap();
        assert_eq!(exchange.len(), 4);
        assert_eq!(exchange[3].content, "The migration is finished.");
    }

    fn origin_of(conn: &Connection, content: &str) -> (Option<String>, bool) {
        conn.query_row(
            "SELECT agent_id, is_meta FROM messages WHERE content = ?1",
            [content],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn test_insert_message_records_origin() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        let origin = MessageOrigin {
            agent_id: Some("agent-1"),
            is_meta: true,
        };
        // Also without a uuid to look the message up by later.
        insert_message(&conn, "s1", None, "user", "no uuid", None, &origin).unwrap();
        assert_eq!(
            origin_of(&conn, "no uuid"),
            (Some("agent-1".to_string()), true)
        );

        // Archived before origins were tracked: importing it again fills it in.
        let plain = MessageOrigin::default();
        assert!(insert_message(&conn, "s1", Some("u1"), "user", "old", None, &plain).unwrap());
        assert!(!insert_message(&conn, "s1", Some("u1"), "user", "old", None, &origin).unwrap());
        assert_eq!(origin_of(&conn, "old"), (Some("agent-1".to_string()), true));
    }

    #[test]
    fn test_open_db_adds_columns_to_old_databases() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(tmp.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                    uuid TEXT, role TEXT NOT NULL, content TEXT NOT NULL, timestamp TEXT);
                 INSERT INTO messages (session_id, uuid, role, content) VALUES
                    ('s1', 'u1', 'user', 'old row'),
                    ('s1', 'u2', 'user', '[Request interrupted by user for tool use]'),
                    ('s1', 'u3', 'user', 'This session is being continued from a previous conversation that ran out of context.'),
                    ('s1', 'u4', 'assistant', '[Request interrupted by user]');
                 CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN SELECT 1; END;",
            )
            .unwrap();
        }
        let conn = open_db(tmp.path()).unwrap();
        let (agent_id, is_meta): (Option<String>, i64) = conn
            .query_row(
                "SELECT agent_id, is_meta FROM messages WHERE uuid = 'u1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((agent_id, is_meta), (None, 0));
        let meta: Vec<String> = conn
            .prepare("SELECT uuid FROM messages WHERE is_meta = 1 ORDER BY uuid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(meta, ["u2", "u3"]);
        let trigger: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(trigger.contains("UPDATE OF content"));
        // Opening again must not try to add the columns twice.
        drop(conn);
        open_db(tmp.path()).unwrap();
    }

    #[test]
    fn test_snippet_ignores_control_characters_in_content() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        // Control characters, and a Nerd Font icon at U+E000.
        let content = "\u{e000} progress \u{1}50%\u{2} done, then the docker build";
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            content,
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let hits = search_exchanges(&conn, "docker", 10, &SearchFilters::default(), false).unwrap();
        assert_eq!(hits.len(), 1);
        let line = &hits[0].matched.as_ref().unwrap().lines[0];
        let at = line.find(MATCH_START).unwrap();
        assert!(line[at..].starts_with(&format!("{MATCH_START}docker{MATCH_END}")));
    }

    #[test]
    fn test_context_around_match() {
        let hit = format!("{MATCH_START}hit{MATCH_END}");
        let marked = format!("start {hit} end");
        assert_eq!(context_around_match(&marked), Some(vec![marked.clone()]));
        assert_eq!(context_around_match("no match here"), None);

        // The lines either side, skipping blank ones.
        let text = format!("one\ntwo\n\n  three {hit}\n\n\tfour\nfive");
        assert_eq!(
            context_around_match(&text).unwrap(),
            [
                "two".to_string(),
                format!("three {hit}"),
                "four".to_string()
            ]
        );
        // Three lines even when the match is in the first or last line.
        let first = format!("{hit}\ntwo\nthree\nfour");
        assert_eq!(
            context_around_match(&first).unwrap(),
            [hit.as_str(), "two", "three"]
        );
        let last = format!("one\ntwo\nthree\n{hit}");
        assert_eq!(
            context_around_match(&last).unwrap(),
            ["two", "three", hit.as_str()]
        );

        // A long matching line is cut so that the match shows; others at the end.
        let long = format!("{} {hit} {}", "a".repeat(100), "b".repeat(400));
        let context = context_around_match(&format!("{}\n{long}", "c".repeat(400))).unwrap();
        let line = &context[1];
        assert!(line.starts_with('…') && line.ends_with('…'));
        assert_eq!(line.chars().filter(|&c| c == 'a').count(), SNIPPET_LEAD - 1);
        assert_eq!(line.chars().count(), SNIPPET_LEN + 2);
        assert!(context[0].starts_with("ccc") && context[0].ends_with('…'));
        assert_eq!(context[0].chars().count(), SNIPPET_LEN + 1);
    }

    #[test]
    fn test_search_exchanges_applies_filters() {
        let (conn, _tmp) = setup_db();
        setup_exchanges(&conn);

        let users = SearchFilters {
            role: Some("user"),
            ..Default::default()
        };
        let hits = search_exchanges(&conn, "docker", 10, &users, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched.as_ref().unwrap().role, "user");

        let later = SearchFilters {
            since: Some("2024-01-01T00:04:00Z"),
            ..Default::default()
        };
        let hits = search_exchanges(&conn, "docker", 10, &later, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].question_preview.as_deref(),
            Some("thanks, now deploy")
        );

        for (project, found) in [
            (ProjectFilter::Containing("other"), false),
            (ProjectFilter::Containing("pro"), true),
            (ProjectFilter::Exact("pro"), false),
            (ProjectFilter::Exact("proj"), true),
        ] {
            let filters = SearchFilters {
                project: Some(project),
                ..Default::default()
            };
            let hits = search_exchanges(&conn, "docker", 10, &filters, false).unwrap();
            assert_eq!(!hits.is_empty(), found, "{project:?}");
        }

        let all = search_exchanges(&conn, "docker", 10, &SearchFilters::default(), false);
        for (session, expected) in [("s1", all.unwrap().len()), ("s2", 0)] {
            let filters = SearchFilters {
                session: Some(session),
                ..Default::default()
            };
            let hits = search_exchanges(&conn, "docker", 10, &filters, false).unwrap();
            assert_eq!(hits.len(), expected, "{session}");
        }
    }

    #[test]
    fn test_get_exchange_stops_at_next_question() {
        let (conn, _tmp) = setup_db();
        let (q1, q2) = setup_exchanges(&conn);
        // Another session's messages interleaved by id must not leak in.
        upsert_session(&conn, "s2", "proj", None).unwrap();
        insert_message(
            &conn,
            "s2",
            Some("x1"),
            "assistant",
            "other session",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("a4"),
            "assistant",
            "Deployed.",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();

        let contents = |question_id| -> Vec<String> {
            get_exchange(&conn, "s1", None, question_id)
                .unwrap()
                .into_iter()
                .map(|m| m.content)
                .collect()
        };
        assert_eq!(
            contents(Some(q1)),
            [
                "how do I fix the docker build?",
                "Let me check the docker logs.",
                "Add --no-cache to the docker build.",
            ]
        );
        assert_eq!(
            contents(Some(q2)),
            [
                "thanks, now deploy",
                "Deploying with docker compose.",
                "Deployed."
            ]
        );
        assert_eq!(contents(None), ["resuming the docker work"]);
    }

    #[test]
    fn test_session_exchanges_lists_every_question() {
        let (conn, _tmp) = setup_db();
        let (q1, q2) = setup_exchanges(&conn);
        let subagent = MessageOrigin {
            agent_id: Some("agent-1"),
            is_meta: false,
        };
        insert_message(
            &conn,
            "s1",
            Some("sq"),
            "user",
            "Read the logs",
            None,
            &subagent,
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("sa"),
            "assistant",
            "Done.",
            None,
            &subagent,
        )
        .unwrap();
        let meta = MessageOrigin {
            agent_id: None,
            is_meta: true,
        };
        insert_message(
            &conn,
            "s1",
            Some("m1"),
            "user",
            "[Request interrupted by user]",
            None,
            &meta,
        )
        .unwrap();

        let mut exchanges: Vec<_> = session_exchanges(&conn, "s1")
            .unwrap()
            .into_iter()
            .map(|e| (e.agent_id, e.question_id, e.question_preview, e.timestamp))
            .collect();
        exchanges.sort();
        let sq: i64 = conn
            .query_row("SELECT id FROM messages WHERE uuid = 'sq'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let text = |s: &str| Some(s.to_string());
        assert_eq!(
            exchanges,
            [
                // The answer that came before any question, from its first message.
                (None, None, None, text("2024-01-01T00:00:00Z")),
                (
                    None,
                    Some(q1),
                    text("how do I fix the docker build?"),
                    text("2024-01-01T00:01:00Z")
                ),
                (
                    None,
                    Some(q2),
                    text("thanks, now deploy"),
                    text("2024-01-01T00:04:00Z")
                ),
                (text("agent-1"), Some(sq), text("Read the logs"), None),
            ]
        );
        let any = session_exchanges(&conn, "s1").unwrap();
        assert!(any
            .iter()
            .all(|e| e.matched.is_none() && e.project == "proj"));
        assert!(session_exchanges(&conn, "missing").unwrap().is_empty());
    }

    #[test]
    fn test_project_sessions() {
        let (conn, _tmp) = setup_db();
        setup_exchanges(&conn);
        upsert_session(&conn, "s2", "proj-old", Some("2023-05-01T00:00:00Z")).unwrap();
        let subagent = MessageOrigin {
            agent_id: Some("agent-1"),
            is_meta: false,
        };
        // Only a subagent asked anything here.
        insert_message(
            &conn,
            "s2",
            Some("x"),
            "user",
            "Read the logs",
            None,
            &subagent,
        )
        .unwrap();
        upsert_session(&conn, "s3", "other", None).unwrap();

        let sessions =
            project_sessions(&conn, Some(ProjectFilter::Exact("proj")), None, None).unwrap();
        assert_eq!(sessions.len(), 1);
        let s1 = &sessions[0];
        assert_eq!((s1.session_id.as_str(), s1.questions), ("s1", 2));
        assert_eq!(
            s1.first_question.as_deref(),
            Some("how do I fix the docker build?")
        );
        // Without a start time, the import time stands in.
        assert!(s1.started.is_some());

        let ids = |project: Option<ProjectFilter>, since, until| -> Vec<String> {
            let mut ids: Vec<String> = project_sessions(&conn, project, since, until)
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .collect();
            ids.sort();
            ids
        };
        let proj = Some(ProjectFilter::Containing("proj"));
        assert_eq!(ids(proj, None, None), ["s1", "s2"]);
        assert_eq!(ids(proj, None, Some("2023-05")), ["s2"]);
        assert_eq!(ids(proj, Some("2024"), None), ["s1"]);
        assert_eq!(ids(None, None, None), ["s1", "s2", "s3"]);

        let s2 =
            project_sessions(&conn, Some(ProjectFilter::Exact("proj-old")), None, None).unwrap();
        assert_eq!(
            (s2[0].questions, s2[0].first_question.as_deref()),
            (1, None)
        );
    }

    #[test]
    fn test_projects_count_sessions() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "a1", "-home-me-app", Some("2024-01-01T00:00:00Z")).unwrap();
        upsert_session(&conn, "a2", "-home-me-app", Some("2024-03-01T00:00:00Z")).unwrap();
        upsert_session(&conn, "t1", "-home-me-tool", Some("2023-06-01T00:00:00Z")).unwrap();

        let summary = |project, since| -> Vec<(String, usize, Option<String>)> {
            let mut found: Vec<_> = projects(&conn, project, since, None)
                .unwrap()
                .into_iter()
                .map(|p| (p.project, p.sessions, p.latest))
                .collect();
            found.sort();
            found
        };
        let app = (
            "-home-me-app".to_string(),
            2,
            Some("2024-03-01T00:00:00Z".to_string()),
        );
        let tool = (
            "-home-me-tool".to_string(),
            1,
            Some("2023-06-01T00:00:00Z".to_string()),
        );
        assert_eq!(summary(None, None), [app.clone(), tool]);
        let recent = ("-home-me-app".to_string(), 1, app.2.clone());
        assert_eq!(summary(Some(ProjectFilter::Containing("app")), None), [app]);
        assert_eq!(summary(None, Some("2024-02")), [recent]);
    }

    #[test]
    fn test_import_times_compare_with_iso_times() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "dated", "proj", Some("2026-01-15T07:00:00Z")).unwrap();
        // Imported later the same day, without timestamps.
        upsert_session(&conn, "imported", "proj", None).unwrap();
        conn.execute(
            "UPDATE sessions SET imported_at = '2026-01-15 18:00:00' WHERE session_id = 'imported'",
            [],
        )
        .unwrap();

        let latest = projects(&conn, None, None, None).unwrap()[0].latest.clone();
        assert_eq!(latest.as_deref(), Some("2026-01-15T18:00:00Z"));
        let since_noon = project_sessions(&conn, None, Some("2026-01-15T12:00"), None).unwrap();
        let ids: Vec<_> = since_noon.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["imported"]);
        let until_noon = project_sessions(&conn, None, None, Some("2026-01-15T12:00")).unwrap();
        let ids: Vec<_> = until_noon.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["dated"]);
        assert_eq!(nth_recent_session_id(&conn, 0).unwrap(), "imported");
    }

    #[test]
    fn test_project_exists_matches_exactly() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "-home-me-app", None).unwrap();
        assert!(project_exists(&conn, "-home-me-app").unwrap());
        assert!(!project_exists(&conn, "-home-me").unwrap());
        assert!(!project_exists(&conn, "-home-me-app-2").unwrap());
    }

    #[test]
    fn test_insert_without_uuid() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        let inserted = insert_message(
            &conn,
            "s1",
            None,
            "user",
            "msg1",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        assert!(inserted);
        // Without uuid, duplicate check is skipped, so second insert also succeeds
        let inserted = insert_message(
            &conn,
            "s1",
            None,
            "user",
            "msg2",
            None,
            &MessageOrigin::default(),
        )
        .unwrap();
        assert!(inserted);

        let (_, count) = stats(&conn).unwrap();
        assert_eq!(count, 2);
    }
}
