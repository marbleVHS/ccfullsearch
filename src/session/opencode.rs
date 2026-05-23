//! Opencode session storage layer (SQLite).
//!
//! Opencode 1.x stores all session state in a single SQLite database at
//! `<opencode_home>/opencode.db`. Older releases used a file tree under
//! `<opencode_home>/storage/{session,message,part}/...`; ccs no longer reads
//! that legacy layout.
//!
//! Database schema (relevant subset):
//!
//! ```sql
//! CREATE TABLE session (
//!     id, project_id, slug, directory, title,
//!     time_created INTEGER, time_updated INTEGER,
//!     ...
//! );
//! CREATE TABLE message (
//!     id, session_id, time_created INTEGER, time_updated INTEGER,
//!     data TEXT  -- JSON: {role, parentID?, time:{created,...}, ...}
//! );
//! CREATE TABLE part (
//!     id, message_id, session_id, time_created INTEGER, time_updated INTEGER,
//!     data TEXT  -- JSON: {type, text?, state?, ...}
//! );
//! CREATE TABLE project ( id, worktree, vcs, name, ... );
//! ```
//!
//! Synthetic file paths: outside this module, sessions are identified by
//! `<opencode_home>/opencode.db#<session_id>`. The `#`-fragment encoding lets
//! the rest of ccs keep a single `file_path: String` field on every record
//! while routing back to the SQLite loader via `is_opencode_session_path`.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::session::record::{ContentBlock, MessageRole, SessionRecord};

const DB_FILENAME: &str = "opencode.db";
const PATH_FRAGMENT_SEP: &str = "#";

/// Resolve the Opencode "home" directory (the parent of `opencode.db`).
///
/// Precedence:
/// 1. `$OPENCODE_DATA` if set and existing
/// 2. `~/.local/share/opencode` (Linux/XDG)
/// 3. `~/Library/Application Support/opencode` (macOS)
pub fn opencode_storage_root() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("OPENCODE_DATA") {
        let path = PathBuf::from(custom);
        if path.exists() {
            return Some(path);
        }
    }
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/share/opencode"),
        home.join("Library/Application Support/opencode"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Path to the Opencode database, if a home directory was found and the file
/// exists on disk.
pub fn opencode_database_path() -> Option<PathBuf> {
    let db = opencode_storage_root()?.join(DB_FILENAME);
    db.exists().then_some(db)
}

/// True when `path` looks like an Opencode synthetic path
/// (`<root>/opencode.db#<session_id>` or just `<root>/opencode.db`).
pub fn is_opencode_session_path(path: &str) -> bool {
    let normalized = if path.contains('\\') {
        path.replace('\\', "/")
    } else {
        path.to_string()
    };
    normalized.contains("/opencode.db")
}

/// Build the synthetic ccs path used to identify an Opencode session.
pub fn synthetic_session_path(db_path: &Path, session_id: &str) -> String {
    format!(
        "{}{}{}",
        db_path.to_string_lossy(),
        PATH_FRAGMENT_SEP,
        session_id
    )
}

/// Extract `(db_path, session_id)` from a synthetic path produced by
/// `synthetic_session_path`. Returns `None` if the path is not Opencode-shaped.
pub fn parse_session_path(path: &str) -> Option<(PathBuf, String)> {
    let (db, sid) = path.rsplit_once(PATH_FRAGMENT_SEP)?;
    if !db.ends_with(DB_FILENAME) {
        return None;
    }
    Some((PathBuf::from(db), sid.to_string()))
}

/// Open a read-only connection to the Opencode database.
///
/// We open `READ_ONLY | URI` and append `?mode=ro` so we never accidentally
/// write to the user's database, even when they have a hot Opencode running.
pub(crate) fn open_db(db_path: &Path) -> rusqlite::Result<Connection> {
    let uri = format!("file:{}?mode=ro", db_path.to_string_lossy());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Metadata for a single Opencode session as stored in the `session` table.
#[derive(Debug, Clone)]
pub struct OpencodeSession {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub directory: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Synthetic path identifying this session for the rest of ccs.
    pub session_file: PathBuf,
}

impl OpencodeSession {
    fn from_row(row: &Row<'_>, db_path: &Path) -> rusqlite::Result<Self> {
        let id: String = row.get("id")?;
        let project_id: String = row.get("project_id")?;
        let title: String = row.get("title")?;
        let directory: Option<String> = row.get("directory")?;
        let time_created: i64 = row.get("time_created")?;
        let time_updated: i64 = row.get("time_updated")?;
        let synthetic = synthetic_session_path(db_path, &id);
        Ok(OpencodeSession {
            id,
            project_id,
            title,
            directory: directory.filter(|s| !s.is_empty()),
            created_at: millis_to_datetime(time_created).unwrap_or_else(Utc::now),
            updated_at: millis_to_datetime(time_updated)
                .unwrap_or_else(|| millis_to_datetime(time_created).unwrap_or_else(Utc::now)),
            session_file: PathBuf::from(&synthetic),
        })
    }

    /// Load a single session by id. Returns `None` if the session does not
    /// exist or the database can't be opened.
    pub fn from_db(db_path: &Path, session_id: &str) -> Option<Self> {
        let conn = open_db(db_path).ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, title, directory, time_created, time_updated \
                 FROM session WHERE id = ?1",
            )
            .ok()?;
        let mut rows = stmt.query([session_id]).ok()?;
        let row = rows.next().ok()??;
        Self::from_row(row, db_path).ok()
    }

    /// Resolve a session from the synthetic `file_path` representation.
    pub fn from_file(path: &Path) -> Option<Self> {
        let path_str = path.to_str()?;
        let (db, sid) = parse_session_path(path_str)?;
        Self::from_db(&db, &sid)
    }
}

/// Look up an Opencode project's display label.
///
/// Returns the project's `name` when set, otherwise the basename of the
/// `worktree` path, otherwise the raw worktree.
pub fn read_project_label(db_path: &Path, project_id: &str) -> Option<String> {
    let conn = open_db(db_path).ok()?;
    let mut stmt = conn
        .prepare("SELECT name, worktree FROM project WHERE id = ?1")
        .ok()?;
    let mut rows = stmt.query([project_id]).ok()?;
    let row = rows.next().ok()??;
    let name: Option<String> = row.get("name").ok();
    let worktree: Option<String> = row.get("worktree").ok();
    if let Some(n) = name.as_ref().filter(|s| !s.is_empty()) {
        return Some(n.clone());
    }
    let worktree = worktree?;
    if let Some(base) = Path::new(&worktree)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
    {
        return Some(base.to_string());
    }
    Some(worktree)
}

/// Read the worktree path for a project (used as the resume cwd).
pub fn read_project_worktree(db_path: &Path, project_id: &str) -> Option<String> {
    let conn = open_db(db_path).ok()?;
    let mut stmt = conn
        .prepare("SELECT worktree FROM project WHERE id = ?1")
        .ok()?;
    let mut rows = stmt.query([project_id]).ok()?;
    let row = rows.next().ok()??;
    row.get::<_, String>("worktree").ok()
}

/// Enumerate every Opencode session at the given DB path. Sessions whose
/// `directory` is empty are skipped (they represent Opencode-internal global
/// state, equivalent to the old `session/global/` bucket).
pub fn list_sessions(db_path: &Path) -> Vec<OpencodeSession> {
    let Ok(conn) = open_db(db_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, project_id, title, directory, time_created, time_updated \
         FROM session \
         WHERE directory IS NOT NULL AND directory != '' \
         ORDER BY time_updated DESC",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |row| OpencodeSession::from_row(row, db_path));
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// A single rendered message from an Opencode session.
#[derive(Debug, Clone)]
pub struct OpencodeMessage {
    pub id: String,
    pub session_id: String,
    pub role: MessageRole,
    pub parent_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub content_blocks: Vec<ContentBlock>,
}

/// Load every message + part for a given session, ordered chronologically.
pub fn load_messages(db_path: &Path, session_id: &str) -> Vec<OpencodeMessage> {
    let Ok(conn) = open_db(db_path) else {
        return Vec::new();
    };

    // Fetch messages first, then fetch parts in a single query and bucket
    // them by message_id. Doing this in two passes (instead of N+1 queries)
    // keeps tree loads fast even for sessions with hundreds of messages.
    let mut msg_stmt = match conn.prepare(
        "SELECT id, session_id, data, time_created \
         FROM message WHERE session_id = ?1 \
         ORDER BY time_created ASC, id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let msg_rows: Vec<(String, String, i64, String)> = msg_stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>("id")?,
                row.get::<_, String>("session_id")?,
                row.get::<_, i64>("time_created")?,
                row.get::<_, String>("data")?,
            ))
        })
        .map(|iter| iter.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();

    if msg_rows.is_empty() {
        return Vec::new();
    }

    // Parts: fetch all rows for this session in one query, sorted by
    // (message_id, time_created, id) so blocks within a message stay in
    // creation order.
    let mut part_stmt = match conn.prepare(
        "SELECT message_id, data \
         FROM part WHERE session_id = ?1 \
         ORDER BY message_id ASC, time_created ASC, id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let part_rows: Vec<(String, String)> = part_stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>("message_id")?,
                row.get::<_, String>("data")?,
            ))
        })
        .map(|iter| iter.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();

    let mut parts_by_msg: std::collections::HashMap<String, Vec<ContentBlock>> =
        std::collections::HashMap::new();
    for (msg_id, data) in part_rows {
        let Ok(json) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        push_blocks_from_part(&json, parts_by_msg.entry(msg_id).or_default());
    }

    let mut out = Vec::with_capacity(msg_rows.len());
    for (id, session_id, time_created, data) in msg_rows {
        let Ok(meta) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let role = match meta.get("role").and_then(|v| v.as_str()) {
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => continue,
        };
        let parent_id = meta
            .get("parentID")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let created_at = meta
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .and_then(millis_to_datetime)
            .unwrap_or_else(|| millis_to_datetime(time_created).unwrap_or_else(Utc::now));
        let content_blocks = parts_by_msg.remove(&id).unwrap_or_default();
        out.push(OpencodeMessage {
            id,
            session_id,
            role,
            parent_id,
            created_at,
            content_blocks,
        });
    }
    out
}

fn push_blocks_from_part(json: &Value, blocks: &mut Vec<ContentBlock>) {
    let Some(part_type) = json.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match part_type {
        "text" => {
            if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    blocks.push(ContentBlock::Text(text.to_string()));
                }
            }
        }
        "reasoning" => {
            if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    blocks.push(ContentBlock::Thinking(text.to_string()));
                }
            }
        }
        "tool" => push_tool_blocks(json, blocks),
        // step-start / step-finish / patch / snapshot are structural,
        // not user-visible content.
        _ => {}
    }
}

fn push_tool_blocks(json: &Value, blocks: &mut Vec<ContentBlock>) {
    let name = json
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("tool")
        .to_string();
    let state = json.get("state");
    let input = state
        .and_then(|s| s.get("input"))
        .map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(v).unwrap_or_default(),
        })
        .unwrap_or_default();
    blocks.push(ContentBlock::ToolUse { name, input });

    if let Some(output) = state.and_then(|s| s.get("output")) {
        let text = match output.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(output).unwrap_or_default(),
        };
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            blocks.push(ContentBlock::ToolResult(text));
        }
    }
}

/// A record paired with its position and timestamp, in the shape expected by
/// `SessionDag::from_records`.
pub type MaterializedRecord = (SessionRecord, usize, Option<DateTime<Utc>>);

/// Materialize an Opencode session as an ordered list of `SessionRecord`s
/// compatible with the existing DAG engine and tree renderer.
///
/// Synthetic chain: Opencode does not branch the way Claude Code does — each
/// assistant turn references the same prior user message via `parentID`. To
/// produce a linear chain that the existing DAG flattener can render as a
/// tree, we link every message to its immediate predecessor in `time.created`
/// order rather than using the recorded `parentID`.
pub fn materialize_session(session: &OpencodeSession) -> Option<(String, Vec<MaterializedRecord>)> {
    let (db, _) = parse_session_path(session.session_file.to_str()?)?;
    let messages = load_messages(&db, &session.id);
    let mut records = Vec::with_capacity(messages.len());
    let mut prev_uuid: Option<String> = None;
    for (idx, msg) in messages.into_iter().enumerate() {
        let parent_uuid = prev_uuid.take();
        let record = SessionRecord::Message {
            role: msg.role,
            content_blocks: msg.content_blocks,
            uuid: Some(msg.id.clone()),
            parent_uuid,
            is_sidechain: false,
        };
        records.push((record, idx, Some(msg.created_at)));
        prev_uuid = Some(msg.id);
    }
    Some((session.id.clone(), records))
}

/// Result row from a full-text scan of `part.data` JSON.
#[derive(Debug, Clone)]
pub struct OpencodeSearchHit {
    pub session_id: String,
    pub message_id: String,
    pub text: String,
}

/// What kind of search the caller wants to run.
///
/// Fixed-string queries use a SQL `LIKE` prefilter so SQLite can narrow the
/// candidate set with its `data` index — orders of magnitude faster than a
/// table scan. Regex queries can't be expressed as a single `LIKE`, so we
/// fall back to a full part scan and let the Rust regex engine do the
/// matching. SQLite has no portable regex operator across builds, and the
/// bundled SQLite we link doesn't load the optional `regex` extension.
#[derive(Debug, Clone, Copy)]
pub enum SearchMode<'a> {
    /// Case-insensitive substring match.
    Fixed,
    /// Full part scan; the caller supplies the compiled regex so we don't
    /// recompile it per row.
    Regex(&'a regex::Regex),
}

/// Scan the `part` table for the given query string. Implemented as a
/// case-insensitive `LIKE` against `data` followed by a JSON-aware extractor
/// in Rust, OR a full table scan when `mode` is `SearchMode::Regex`.
///
/// The post-filter is necessary because the `LIKE` runs against the raw JSON
/// envelope, which means a query that happens to match a key name (`"text"`,
/// `"tool"`) would otherwise produce false positives.
pub fn search_parts(db_path: &Path, query: &str, mode: SearchMode<'_>) -> Vec<OpencodeSearchHit> {
    let Ok(conn) = open_db(db_path) else {
        return Vec::new();
    };

    // For fixed-string queries, narrow the candidate set with a LIKE
    // prefilter. For regex queries, the LIKE can't express the pattern
    // (e.g. `hello\s+opencode` would become a literal LIKE pattern and
    // miss `hello opencode`), so scan all parts and let the Rust regex
    // engine do the matching after JSON extraction.
    let (sql, params): (&str, Vec<String>) = match mode {
        SearchMode::Fixed => (
            "SELECT session_id, message_id, data FROM part \
             WHERE LOWER(data) LIKE ?1 ESCAPE '\\' \
             ORDER BY time_created DESC",
            vec![format!("%{}%", escape_like(&query.to_lowercase()))],
        ),
        SearchMode::Regex(_) => (
            "SELECT session_id, message_id, data FROM part ORDER BY time_created DESC",
            vec![],
        ),
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>("session_id")?,
            row.get::<_, String>("message_id")?,
            row.get::<_, String>("data")?,
        ))
    });
    let Ok(iter) = rows else {
        return Vec::new();
    };

    let lower_query = query.to_lowercase();
    let mut hits = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in iter.filter_map(Result::ok) {
        let (session_id, message_id, data) = row;
        let Ok(json) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let Some(text) = render_part_text(&json) else {
            continue;
        };
        let matched = match mode {
            SearchMode::Fixed => text.to_lowercase().contains(&lower_query),
            SearchMode::Regex(re) => re.is_match(&text),
        };
        if !matched {
            continue;
        }
        // Collapse multiple parts of the same message into one hit to mirror
        // ripgrep's behaviour (one match per session/message pair surfaces in
        // the UI; users expand the session to see all hits).
        if !seen.insert(format!("{}:{}", session_id, message_id)) {
            continue;
        }
        hits.push(OpencodeSearchHit {
            session_id,
            message_id,
            text,
        });
    }
    hits
}

/// Render the user-visible text of a part for the search post-filter.
/// Returns `None` for structural parts (`step-start`, `step-finish`,
/// `snapshot`, `patch`) so they're skipped as match candidates.
pub fn render_part_text(json: &Value) -> Option<String> {
    let part_type = json.get("type").and_then(|v| v.as_str())?;
    match part_type {
        "text" | "reasoning" => json
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "tool" => {
            let state = json.get("state")?;
            let mut out = String::new();
            if let Some(input) = state.get("input") {
                let rendered = match input.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_json::to_string(input).unwrap_or_default(),
                };
                out.push_str(&rendered);
            }
            if let Some(output) = state.get("output") {
                let rendered = match output.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_json::to_string(output).unwrap_or_default(),
                };
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&rendered);
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

/// Look up a single message's role + created_at without loading its parts.
/// Used by the search layer to label individual match rows.
pub fn read_message_role_and_time(
    db_path: &Path,
    message_id: &str,
) -> Option<(MessageRole, DateTime<Utc>)> {
    let conn = open_db(db_path).ok()?;
    let mut stmt = conn
        .prepare("SELECT data, time_created FROM message WHERE id = ?1")
        .ok()?;
    let mut rows = stmt.query([message_id]).ok()?;
    let row = rows.next().ok()??;
    let data: String = row.get("data").ok()?;
    let time_created: i64 = row.get("time_created").ok()?;
    let json: Value = serde_json::from_str(&data).ok()?;
    let role = match json.get("role").and_then(|v| v.as_str())? {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => return None,
    };
    let ts = json
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(|v| v.as_i64())
        .and_then(millis_to_datetime)
        .unwrap_or_else(|| millis_to_datetime(time_created).unwrap_or_else(Utc::now));
    Some((role, ts))
}

fn millis_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    if ms < 0 {
        return None;
    }
    let secs = ms / 1000;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a minimal Opencode SQLite database on disk and return
    /// `(tempdir, db_path)`.
    pub(crate) fn build_db_fixture() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE project (
                id TEXT PRIMARY KEY,
                worktree TEXT NOT NULL,
                vcs TEXT,
                name TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                slug TEXT NOT NULL DEFAULT '',
                directory TEXT,
                title TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            "#,
        )
        .unwrap();

        // One project with two sessions; one of those is a no-directory
        // "global" session that list_sessions must skip.
        conn.execute(
            "INSERT INTO project VALUES ('projAAA', '/tmp/myproj', 'git', 'myproj', 1, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_TEST', 'projAAA', 'curious-star', '/tmp/myproj', \
             'Test conversation', 1769762431585, 1769762509666)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_GLOBAL', 'projAAA', 'g', '', 'global noise', 1, 2)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO message VALUES ('msg_user1', 'ses_TEST', 1769762431591, 1769762431591, ?1)",
            [r#"{"role":"user","time":{"created":1769762431591}}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_asst1', 'ses_TEST', 1769762431596, 1769762431596, ?1)",
            [r#"{"role":"assistant","parentID":"msg_user1","time":{"created":1769762431596}}"#],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO part VALUES ('prt_001', 'msg_user1', 'ses_TEST', 1, 1, ?1)",
            [r#"{"type":"text","text":"Hello opencode"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_002', 'msg_asst1', 'ses_TEST', 2, 2, ?1)",
            [r#"{"type":"step-start","snapshot":"abc"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_003', 'msg_asst1', 'ses_TEST', 3, 3, ?1)",
            [r#"{"type":"text","text":"Sure thing"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_004', 'msg_asst1', 'ses_TEST', 4, 4, ?1)",
            [r#"{"type":"tool","tool":"grep","state":{"input":{"pattern":"foo"},"output":"matched 3 lines"}}"#],
        )
        .unwrap();

        (dir, db_path)
    }

    #[test]
    fn test_is_opencode_session_path_detects_db() {
        assert!(is_opencode_session_path(
            "/home/u/.local/share/opencode/opencode.db#ses_x"
        ));
        assert!(is_opencode_session_path(
            "/home/u/.local/share/opencode/opencode.db"
        ));
        assert!(!is_opencode_session_path(
            "/home/u/.claude/projects/foo/abc.jsonl"
        ));
    }

    #[test]
    fn test_parse_session_path_round_trip() {
        let db = PathBuf::from("/x/opencode.db");
        let synth = synthetic_session_path(&db, "ses_42");
        let (parsed_db, sid) = parse_session_path(&synth).unwrap();
        assert_eq!(parsed_db, db);
        assert_eq!(sid, "ses_42");
        assert!(parse_session_path("/not/even/a/db.jsonl#abc").is_none());
    }

    #[test]
    fn test_list_sessions_skips_empty_directory() {
        let (_dir, db) = build_db_fixture();
        let sessions = list_sessions(&db);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_TEST");
        assert_eq!(sessions[0].title, "Test conversation");
        assert_eq!(sessions[0].directory.as_deref(), Some("/tmp/myproj"));
    }

    #[test]
    fn test_session_from_file_uses_synthetic_path() {
        let (_dir, db) = build_db_fixture();
        let synth = synthetic_session_path(&db, "ses_TEST");
        let session = OpencodeSession::from_file(Path::new(&synth)).unwrap();
        assert_eq!(session.id, "ses_TEST");
        assert_eq!(session.project_id, "projAAA");
        assert_eq!(session.title, "Test conversation");
    }

    #[test]
    fn test_read_project_label_prefers_name() {
        let (_dir, db) = build_db_fixture();
        // name was inserted as 'myproj' so it should win over the worktree basename
        assert_eq!(
            read_project_label(&db, "projAAA").as_deref(),
            Some("myproj")
        );
    }

    #[test]
    fn test_load_messages_orders_by_time_and_renders_parts() {
        let (_dir, db) = build_db_fixture();
        let messages = load_messages(&db, "ses_TEST");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, "msg_user1");
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(
            messages[0].content_blocks,
            vec![ContentBlock::Text("Hello opencode".to_string())]
        );
        assert_eq!(messages[1].id, "msg_asst1");
        assert!(matches!(
            messages[1].content_blocks.first(),
            Some(ContentBlock::Text(_))
        ));
        // tool + tool_result emitted from a single tool part
        assert!(messages[1]
            .content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "grep")));
        assert!(messages[1]
            .content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult(_))));
    }

    #[test]
    fn test_materialize_session_builds_linear_chain() {
        let (_dir, db) = build_db_fixture();
        let session =
            OpencodeSession::from_file(Path::new(&synthetic_session_path(&db, "ses_TEST")))
                .unwrap();
        let (sid, records) = materialize_session(&session).unwrap();
        assert_eq!(sid, "ses_TEST");
        assert_eq!(records.len(), 2);

        match &records[1].0 {
            SessionRecord::Message {
                uuid, parent_uuid, ..
            } => {
                assert_eq!(uuid.as_deref(), Some("msg_asst1"));
                assert_eq!(
                    parent_uuid.as_deref(),
                    Some("msg_user1"),
                    "second message must chain to the first"
                );
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn test_search_parts_returns_text_hits() {
        let (_dir, db) = build_db_fixture();
        let hits = search_parts(&db, "Hello opencode", SearchMode::Fixed);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "ses_TEST");
        assert_eq!(hits[0].message_id, "msg_user1");
        assert!(hits[0].text.contains("Hello opencode"));
    }

    #[test]
    fn test_search_parts_finds_tool_output() {
        let (_dir, db) = build_db_fixture();
        let hits = search_parts(&db, "matched 3 lines", SearchMode::Fixed);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "msg_asst1");
    }

    #[test]
    fn test_search_parts_ignores_structural_match() {
        // The step-start part has snapshot=abc; a query for "abc" hits the
        // JSON envelope via LIKE but render_part_text returns None for
        // step-start, so the post-filter drops it.
        let (_dir, db) = build_db_fixture();
        let hits = search_parts(&db, "abc", SearchMode::Fixed);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_parts_case_insensitive() {
        let (_dir, db) = build_db_fixture();
        let hits = search_parts(&db, "HELLO OPENCODE", SearchMode::Fixed);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_search_parts_regex_matches_whitespace_pattern() {
        // The regression the LIKE-only prefilter caused: `hello\s+opencode`
        // would be turned into a literal LIKE pattern and never match the
        // text `Hello opencode` (which has a real space, not `\s+`).
        let (_dir, db) = build_db_fixture();
        let re = regex::RegexBuilder::new(r"hello\s+opencode")
            .case_insensitive(true)
            .build()
            .unwrap();
        let hits = search_parts(&db, r"hello\s+opencode", SearchMode::Regex(&re));
        assert_eq!(hits.len(), 1, "regex must match across the whitespace");
        assert_eq!(hits[0].session_id, "ses_TEST");
    }

    #[test]
    fn test_search_parts_regex_does_not_short_circuit_on_envelope() {
        // The LIKE prefilter happens to match JSON keys like `"text"` for the
        // query `text`, but a regex search should rely on render_part_text
        // (user-visible content) — not on envelope substrings. Pattern
        // `^Hello` should match only the user message, not the assistant's
        // `Sure thing` or the tool output.
        let (_dir, db) = build_db_fixture();
        let re = regex::Regex::new("^Hello").unwrap();
        let hits = search_parts(&db, "^Hello", SearchMode::Regex(&re));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "msg_user1");
    }
}
