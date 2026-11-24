use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::db;

/// Low-value tool_use calls that only reference file paths or simple lookups.
/// These are skipped to reduce noise in the archive.
const SKIP_TOOL_NAMES: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "ToolSearch",
    "mcp__playwright__browser_snapshot",
    "mcp__playwright__browser_take_screenshot",
    "mcp__playwright__browser_navigate",
    "mcp__playwright__browser_click",
    "mcp__playwright__browser_tabs",
    "TaskGet",
    "TaskOutput",
    "TaskList",
];

fn extract_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => item.get("text").and_then(|t| t.as_str()).map(String::from),
                    Some("tool_use") => {
                        let name = item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown");
                        // Skip low-value tool calls
                        if SKIP_TOOL_NAMES.contains(&name) {
                            return None;
                        }
                        let input = item.get("input").map(|i| i.to_string()).unwrap_or_default();
                        Some(format!("[tool_use: {name}] {input}"))
                    }
                    Some("tool_result") => None,
                    _ => None,
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Remove system-injected noise tags from message content.
/// The slash command a user message ran, as typed ("/review src/auth"), from
/// the `<command-name>` and `<command-args>` tags Claude Code records it as.
fn command_line(content: &str) -> Option<String> {
    let tag = |name: &str| {
        let start = content.find(&format!("<{name}>"))? + name.len() + 2;
        let end = start + content[start..].find(&format!("</{name}>"))?;
        Some(content[start..end].trim())
    };
    let name = tag("command-name").filter(|name| !name.is_empty())?;
    let slash = if name.starts_with('/') { "" } else { "/" };
    let line = match tag("command-args") {
        Some(args) if !args.is_empty() => format!("{slash}{name} {args}"),
        _ => format!("{slash}{name}"),
    };
    Some(line)
}

fn clean_content(content: &str) -> String {
    let mut result = content.to_string();

    // Remove XML-like system tags and their content
    let tag_patterns = [
        "system-reminder",
        "local-command-caveat",
        "local-command-stdout",
        "command-name",
        "command-message",
        "command-args",
        "task-notification",
    ];
    for tag in &tag_patterns {
        while let Some(start) = result.find(&format!("<{tag}>")) {
            if let Some(end) = result.find(&format!("</{tag}>")) {
                let end = end + format!("</{tag}>").len();
                result.replace_range(start..end, "");
            } else {
                break;
            }
        }
        // Also remove self-closing or opening-only variants
        while let Some(start) = result.find(&format!("<{tag}")) {
            if let Some(end) = result[start..].find('>') {
                result.replace_range(start..start + end + 1, "");
            } else {
                break;
            }
        }
    }

    result
}

/// Check if a message is noise that should be skipped entirely.
fn is_noise(content: &str) -> bool {
    let trimmed = content.trim();

    // Zero-width spaces, empty, or trivially short non-meaningful
    if trimmed.is_empty() {
        return true;
    }
    if trimmed
        .chars()
        .all(|c| c.is_whitespace() || c == '\u{200b}' || c == '\u{feff}')
    {
        return true;
    }

    // Stop hook / eval-loop meta messages
    if trimmed.contains("Stop hook feedback:") {
        return true;
    }
    if trimmed.starts_with("eval-loop iteration") {
        return true;
    }
    if trimmed.starts_with("# eval-loop Command") {
        return true;
    }

    false
}

/// Where each subagent's chain of records starts. Subagents recorded inside their
/// session's transcript, before `agentId` existed, are told apart by following
/// `parentUuid` links back to the first record of their chain.
#[derive(Default)]
struct SidechainThreads {
    root_of: HashMap<String, String>,
}

impl SidechainThreads {
    /// Note a sidechain record. All of them count, archived or not: chains also run
    /// through tool results and attachments.
    fn track(&mut self, record: &Value) {
        let Some(uuid) = record.get("uuid").and_then(Value::as_str) else {
            return;
        };
        let root = record
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|parent| self.root_of.get(parent))
            .cloned()
            .unwrap_or_else(|| uuid.to_string());
        self.root_of.insert(uuid.to_string(), root);
    }

    /// The uuid of the first record in this record's chain.
    fn root(&self, record: &Value) -> Option<&str> {
        let uuid = record.get("uuid").and_then(Value::as_str)?;
        self.root_of.get(uuid).map(String::as_str)
    }
}

/// Which subagent wrote a record, and whether Claude Code (not the user) added it.
fn message_origin<'a>(
    record: &'a Value,
    role: &str,
    content: &str,
    threads: &'a SidechainThreads,
) -> db::MessageOrigin<'a> {
    let flag = |key: &str| record.get(key).and_then(Value::as_bool).unwrap_or(false);
    let sidechain = flag("isSidechain");
    let root = threads.root(record);
    // Subagent transcripts share their parent's sessionId; tell them apart by
    // agentId, or by where their chain starts.
    let agent_id = sidechain.then(|| {
        record
            .get("agentId")
            .and_then(Value::as_str)
            .or(root)
            .unwrap_or("sidechain")
    });
    // The prompt that starts a subagent is its question, even when flagged as meta.
    let opens_subagent = sidechain
        && match root {
            Some(root) => record.get("uuid").and_then(Value::as_str) == Some(root),
            None => record.get("parentUuid").is_none_or(Value::is_null),
        };
    db::MessageOrigin {
        agent_id,
        is_meta: !opens_subagent
            && (flag("isMeta") || flag("isCompactSummary") || db::is_meta_text(role, content)),
    }
}

pub fn import_jsonl_file(conn: &Connection, path: &Path, project: &str) -> Result<ImportStats> {
    let file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut stats = ImportStats::default();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;

    let mut threads = SidechainThreads::default();
    // The slash command the last user message ran ("/review src/auth"). Its
    // record is filtered as empty, so the prompt it expands to, flagged as meta,
    // is the question.
    let mut command: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let record: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        let record_type = match record.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };

        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            threads.track(&record);
        }

        // Extract session ID from any record that has it
        if session_id.is_none() {
            if let Some(sid) = record.get("sessionId").and_then(|s| s.as_str()) {
                session_id = Some(sid.to_string());
            }
        }

        match record_type {
            "user" | "assistant" => {
                let ran = command.take();
                let message = match record.get("message") {
                    Some(m) => m,
                    None => continue,
                };

                let role = match message.get("role").and_then(|r| r.as_str()) {
                    Some(r) => r,
                    None => continue,
                };

                let raw_content = match message.get("content").and_then(extract_text_content) {
                    Some(c) => c,
                    None => continue,
                };
                if role == "user" {
                    command = command_line(&raw_content);
                }

                // Clean and filter content
                let mut content = clean_content(&raw_content);
                if is_noise(&content) {
                    stats.filtered += 1;
                    continue;
                }

                // Skip empty content after cleaning
                if content.trim().is_empty() {
                    stats.filtered += 1;
                    continue;
                }

                let uuid = record.get("uuid").and_then(|u| u.as_str());
                let timestamp = record.get("timestamp").and_then(|t| t.as_str());

                if first_timestamp.is_none() {
                    first_timestamp = timestamp.map(String::from);
                }

                let sid = match &session_id {
                    Some(s) => s.as_str(),
                    None => {
                        // Derive session ID from filename
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown");
                        session_id = Some(stem.to_string());
                        session_id.as_ref().unwrap().as_str()
                    }
                };

                db::upsert_session(conn, sid, project, first_timestamp.as_deref())?;

                let mut origin = message_origin(&record, role, &content, &threads);
                // A local command (/model, /effort) is followed by its output
                // instead, which isn't flagged meta.
                let expanded = role == "user"
                    && origin.agent_id.is_none()
                    && record.get("isMeta").and_then(Value::as_bool) == Some(true);
                let command_question = match ran {
                    Some(ran) if expanded => {
                        origin.is_meta = false;
                        content = format!("{ran}\n\n{content}");
                        true
                    }
                    _ => false,
                };
                if db::insert_message(conn, sid, uuid, role, &content, timestamp, &origin)? {
                    stats.imported += 1;
                } else {
                    stats.skipped += 1;
                    // Archived as meta by older versions: make it the question.
                    if let (true, Some(uuid)) = (command_question, uuid) {
                        db::set_message_origin(conn, uuid, &origin)?;
                    }
                }
            }
            _ => {
                // Skip non-message types (file-history-snapshot, progress, queue-operation, system)
            }
        }
    }

    Ok(stats)
}

#[derive(Default, Debug)]
pub struct ImportStats {
    /// New messages archived.
    pub imported: usize,
    /// Messages archived before.
    pub skipped: usize,
    pub filtered: usize,
    pub errors: usize,
    /// Files read, by `import_all`.
    pub files: usize,
}

/// The name of the directory in `~/.claude/projects` where Claude Code keeps the
/// transcripts of sessions started in `dir`, and so the project `import` archives
/// them under: the path with each character other than an ASCII letter or digit
/// replaced by '-' (two for characters JavaScript stores as two UTF-16 units).
pub fn project_dir_name(dir: &Path) -> String {
    encode_path(&dir.to_string_lossy())
}

fn encode_path(path: &str) -> String {
    let mut name = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            name.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    name
}

/// Deepest directory `project_dir` looks into, in case of symlink loops.
const MAX_PROJECT_DEPTH: usize = 64;

/// The directory whose `project_dir_name` is `name`, if one exists: found by
/// walking down from the root, through the entries whose names encode as the
/// next part of `name`.
pub fn project_dir(name: &str) -> Option<PathBuf> {
    let rest = name.strip_prefix('-')?;
    find_project_dir(Path::new("/"), rest, 0)
}

/// The directory below `dir` encoded as `rest`.
fn find_project_dir(dir: &Path, rest: &str, depth: usize) -> Option<PathBuf> {
    if depth > MAX_PROJECT_DEPTH {
        return None;
    }
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    // In a stable order, so the same directory is found each time.
    names.sort();
    for name in names {
        let Some(after) = rest.strip_prefix(&encode_path(&name)) else {
            continue;
        };
        let path = dir.join(&name);
        if !path.is_dir() {
            continue;
        }
        if after.is_empty() {
            return Some(path);
        }
        if let Some(found) = after
            .strip_prefix('-')
            .and_then(|deeper| find_project_dir(&path, deeper, depth + 1))
        {
            return Some(found);
        }
    }
    None
}

pub fn discover_jsonl_files(claude_dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        anyhow::bail!("Projects directory not found: {}", projects_dir.display());
    }

    let mut files = Vec::new();

    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let project_name = entry.file_name().to_string_lossy().to_string();

        // Find .jsonl files in the project directory and subagent directories
        collect_jsonl_files(&project_dir, &project_name, &mut files)?;
    }

    Ok(files)
}

fn collect_jsonl_files(
    dir: &Path,
    project_name: &str,
    files: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    for file_entry in fs::read_dir(dir)? {
        let file_entry = file_entry?;
        let file_path = file_entry.path();
        if file_path.is_file() && file_path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push((file_path, project_name.to_string()));
        } else if file_path.is_dir() {
            // Recurse into subdirectories (e.g., session_id/subagents/)
            collect_jsonl_files(&file_path, project_name, files)?;
        }
    }
    Ok(())
}

pub fn import_all(conn: &Connection, claude_dir: &Path) -> Result<ImportStats> {
    let files = discover_jsonl_files(claude_dir)?;
    let mut total = ImportStats {
        files: files.len(),
        ..ImportStats::default()
    };

    let file_count = files.len();
    for (i, (path, project)) in files.iter().enumerate() {
        if file_count > 10 && (i + 1) % 10 == 0 {
            eprint!("\rProcessing {}/{} files...", i + 1, file_count);
        }
        match import_jsonl_file(conn, path, project) {
            Ok(stats) => {
                total.imported += stats.imported;
                total.skipped += stats.skipped;
                total.filtered += stats.filtered;
                total.errors += stats.errors;
            }
            Err(e) => {
                eprintln!("Warning: Failed to import {}: {}", path.display(), e);
                total.errors += 1;
            }
        }
    }
    if file_count > 10 {
        eprintln!("\rProcessing {}/{} files... done.", file_count, file_count);
    }

    println!(
        "Imported {} messages ({} skipped, {} filtered, {} errors) from {} files",
        total.imported, total.skipped, total.filtered, total.errors, total.files
    );

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn setup_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = db::open_db(tmp.path()).unwrap();
        (conn, tmp)
    }

    #[test]
    fn test_extract_text_content_string() {
        let v = Value::String("hello".into());
        assert_eq!(extract_text_content(&v), Some("hello".into()));
    }

    #[test]
    fn test_extract_text_content_array() {
        let v: Value = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(extract_text_content(&v), Some("hello\nworld".into()));
    }

    #[test]
    fn test_import_jsonl_file() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"hello world"}}}}"#).unwrap();
        writeln!(jsonl, r#"{{"type":"assistant","sessionId":"s1","uuid":"u2","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"hi there"}}}}"#).unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"file-history-snapshot","messageId":"x"}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "test-project").unwrap();
        assert_eq!(stats.imported, 2);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.errors, 0);

        let (sessions, messages) = db::stats(&conn).unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(messages, 2);
    }

    fn origin_of(conn: &Connection, uuid: &str) -> (Option<String>, bool) {
        conn.query_row(
            "SELECT agent_id, is_meta FROM messages WHERE uuid = ?1",
            [uuid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn test_import_records_message_origin() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","sessionId":"s1","uuid":"u1","message":{"role":"user","content":"real question"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u2","isMeta":true,"message":{"role":"user","content":"Base directory for this skill"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u3","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u4","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u5","isSidechain":true,"agentId":"a1","parentUuid":null,"isMeta":true,"message":{"role":"user","content":"subagent task"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"u6","isSidechain":true,"message":{"role":"assistant","content":"old-style sidechain"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u7","isSidechain":true,"agentId":"a1","parentUuid":"u5","isMeta":true,"message":{"role":"user","content":"skill text loaded by the subagent"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"u8","message":{"role":"assistant","content":"[Request interrupted by user] is what Claude Code writes"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u9","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context."}}"#,
        ] {
            writeln!(jsonl, "{line}").unwrap();
        }
        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 9);

        assert_eq!(origin_of(&conn, "u1"), (None, false));
        assert_eq!(origin_of(&conn, "u2"), (None, true));
        assert_eq!(origin_of(&conn, "u3"), (None, true));
        assert_eq!(origin_of(&conn, "u4"), (None, true));
        assert_eq!(origin_of(&conn, "u5"), (Some("a1".to_string()), false));
        assert_eq!(origin_of(&conn, "u6"), (Some("u6".to_string()), false));
        assert_eq!(origin_of(&conn, "u7"), (Some("a1".to_string()), true));
        // Text-based rules apply to user messages only, flags or not.
        assert_eq!(origin_of(&conn, "u8"), (None, false));
        assert_eq!(origin_of(&conn, "u9"), (None, true));
    }

    #[test]
    fn test_import_makes_slash_commands_questions() {
        let (conn, _tmp) = setup_db();
        let mut jsonl = NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","sessionId":"s1","uuid":"u1","message":{"role":"user","content":"fix the login bug"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"u2","message":{"role":"assistant","content":"Fixed."}}"#,
            // A command whose prompt Claude Code expands: the prompt is the question.
            r#"{"type":"user","sessionId":"s1","uuid":"u3","message":{"role":"user","content":"<command-message>review is running…</command-message>\n<command-name>/review</command-name>\n<command-args>src/auth</command-args>"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u4","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"Review the code in src/auth."}]}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"u5","message":{"role":"assistant","content":"Looks fine."}}"#,
            // A local command, with its output: no question.
            r#"{"type":"user","sessionId":"s1","uuid":"u6","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u7","message":{"role":"user","content":"<local-command-stdout>Set model to opus</local-command-stdout>"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"u8","isMeta":true,"message":{"role":"user","content":"Base directory for this skill"}}"#,
        ] {
            writeln!(jsonl, "{line}").unwrap();
        }
        import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();

        assert_eq!(origin_of(&conn, "u4"), (None, false));
        let question: String = conn
            .query_row("SELECT content FROM messages WHERE uuid = 'u4'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(question, "/review src/auth\n\nReview the code in src/auth.");
        assert_eq!(origin_of(&conn, "u8"), (None, true));
        let exchanges = db::session_exchanges(&conn, "s1").unwrap();
        assert_eq!(exchanges.len(), 2);

        // Archived as meta by an older version: importing again fixes it.
        conn.execute("UPDATE messages SET is_meta = 1 WHERE uuid = 'u4'", [])
            .unwrap();
        import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(origin_of(&conn, "u4"), (None, false));

        assert_eq!(
            command_line("<command-name>init</command-name>").as_deref(),
            Some("/init")
        );
        assert_eq!(command_line("plain text"), None);
    }

    #[test]
    fn test_import_separates_inline_subagents() {
        let (conn, _tmp) = setup_db();

        // An older transcript: two subagents recorded inside the session's own file,
        // without agentId, their records interleaved.
        let mut jsonl = NamedTempFile::new().unwrap();
        for line in [
            r#"{"type":"user","sessionId":"s1","uuid":"m1","parentUuid":null,"message":{"role":"user","content":"main question"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"a1","parentUuid":"m1","isSidechain":true,"isMeta":true,"message":{"role":"user","content":"task A"}}"#,
            r#"{"type":"user","sessionId":"s1","uuid":"b1","parentUuid":"m1","isSidechain":true,"message":{"role":"user","content":"task B"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a2","parentUuid":"a1","isSidechain":true,"message":{"role":"assistant","content":"working on A"}}"#,
            // A tool result: not archived, but the chain runs through it.
            r#"{"type":"user","sessionId":"s1","uuid":"a3","parentUuid":"a2","isSidechain":true,"message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"b2","parentUuid":"b1","isSidechain":true,"message":{"role":"assistant","content":"done with B"}}"#,
            r#"{"type":"assistant","sessionId":"s1","uuid":"a4","parentUuid":"a3","isSidechain":true,"message":{"role":"assistant","content":"done with A"}}"#,
        ] {
            writeln!(jsonl, "{line}").unwrap();
        }
        import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();

        let thread = |id: &str| (Some(id.to_string()), false);
        assert_eq!(origin_of(&conn, "m1"), (None, false));
        // Its opening prompt is subagent A's question, even though flagged meta.
        assert_eq!(origin_of(&conn, "a1"), thread("a1"));
        assert_eq!(origin_of(&conn, "a2"), thread("a1"));
        assert_eq!(origin_of(&conn, "a4"), thread("a1"));
        assert_eq!(origin_of(&conn, "b1"), thread("b1"));
        assert_eq!(origin_of(&conn, "b2"), thread("b1"));
    }

    #[test]
    fn test_reimport_fills_in_origin_of_archived_messages() {
        let (conn, _tmp) = setup_db();
        // Archived by a version that didn't record origins.
        db::upsert_session(&conn, "s1", "proj", None).unwrap();
        let plain = db::MessageOrigin::default();
        db::insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "subagent task",
            None,
            &plain,
        )
        .unwrap();
        assert_eq!(origin_of(&conn, "u1"), (None, false));

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, r#"{{"type":"user","sessionId":"s1","uuid":"u1","isSidechain":true,"agentId":"a1","message":{{"role":"user","content":"subagent task"}}}}"#).unwrap();
        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(origin_of(&conn, "u1"), (Some("a1".to_string()), false));
    }

    #[test]
    fn test_import_skips_duplicates() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"hello"}}}}"#).unwrap();

        import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn test_discover_jsonl_files() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("my-project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("session1.jsonl"), "{}").unwrap();
        fs::write(project.join("session2.jsonl"), "{}").unwrap();
        fs::write(project.join("not-jsonl.txt"), "{}").unwrap();

        // subagents dir is now also scanned recursively
        let subagents = project.join("session1").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent.jsonl"), "{}").unwrap();

        let files = discover_jsonl_files(dir.path()).unwrap();
        assert_eq!(files.len(), 3); // 2 top-level + 1 subagent
        assert!(files.iter().all(|(_, name)| name == "my-project"));
    }

    #[test]
    fn test_project_dir_name_matches_claude_code() {
        let name = |path: &str| project_dir_name(Path::new(path));
        assert_eq!(
            name("/home/ubuntu/claude-scope"),
            "-home-ubuntu-claude-scope"
        );
        assert_eq!(name("/home/me/my app/v1.2_x"), "-home-me-my-app-v1-2-x");
        assert_eq!(name("/home/me/café"), "-home-me-caf-");
        assert_eq!(name("/home/me/🦀"), "-home-me---");
    }

    #[cfg(unix)]
    #[test]
    fn test_project_dir_reverses_project_dir_name() {
        let tmp = tempfile::tempdir().unwrap();
        // `/tmp` may be a symlink (macOS), which `project_dir` doesn't resolve.
        let root = tmp.path().canonicalize().unwrap();
        let app = root.join("my app").join("v1.2_x");
        let hidden = root.join(".config").join("tool");
        let lookalike = root.join("my").join("app");
        for dir in [&app, &hidden, &lookalike] {
            fs::create_dir_all(dir).unwrap();
        }
        // A file whose name encodes the same as a directory's is passed over.
        fs::write(root.join("my-app"), "").unwrap();
        for dir in [&app, &hidden, &lookalike] {
            assert_eq!(project_dir(&project_dir_name(dir)).as_ref(), Some(dir));
        }
        assert_eq!(project_dir(&project_dir_name(&root.join("gone"))), None);
        assert_eq!(project_dir("relative"), None);
    }

    #[test]
    fn test_extract_tool_use_skips_read() {
        // Read is a low-value tool, should be skipped
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"path": "/tmp/foo"}},
            {"type": "text", "text": "done"}
        ]);
        let result = extract_text_content(&v).unwrap();
        assert!(!result.contains("Read"));
        assert!(result.contains("done"));
    }

    #[test]
    fn test_extract_tool_use_keeps_edit() {
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Edit", "input": {"file": "foo.rs", "content": "fn main() {}"}}
        ]);
        let result = extract_text_content(&v).unwrap();
        assert!(result.contains("[tool_use: Edit]"));
    }

    #[test]
    fn test_extract_read_only_returns_none() {
        // A message with only a Read tool_use should produce None
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"path": "/tmp/foo"}}
        ]);
        assert_eq!(extract_text_content(&v), None);
    }

    #[test]
    fn test_extract_empty_array() {
        let v: Value = serde_json::json!([]);
        assert_eq!(extract_text_content(&v), None);
    }

    #[test]
    fn test_clean_content_removes_system_reminder() {
        let input = "hello <system-reminder>noise here</system-reminder> world";
        assert_eq!(clean_content(input), "hello  world");
    }

    #[test]
    fn test_clean_content_removes_caveat() {
        let input = "<local-command-caveat>blah</local-command-caveat>real content";
        assert_eq!(clean_content(input), "real content");
    }

    #[test]
    fn test_is_noise() {
        assert!(is_noise(""));
        assert!(is_noise("\u{200b}"));
        assert!(is_noise("Stop hook feedback: some task"));
        assert!(is_noise("some prefix\nStop hook feedback: task"));
        assert!(is_noise("eval-loop iteration 3/30."));
        assert!(is_noise("# eval-loop Command\n\nExecute the setup..."));
        assert!(!is_noise("hello world"));
        // Messages discussing eval-loop should NOT be filtered
        assert!(!is_noise(
            "The eval_commands field contains the test scripts"
        ));
        assert!(!is_noise(
            "Check the eval-loop iteration results in the output"
        ));
    }

    #[test]
    fn test_clean_content_removes_command_tags() {
        let input = "<command-name>/mcp</command-name>\n<command-message>mcp</command-message>\n<command-args></command-args>";
        let cleaned = clean_content(input);
        assert!(!cleaned.contains("command-name"));
        assert!(cleaned.trim().is_empty());
    }

    #[test]
    fn test_import_empty_content_skipped() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","message":{{"role":"user","content":"   "}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 0);
    }

    #[test]
    fn test_import_filters_system_reminder() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        // Message that is purely a system-reminder
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"<system-reminder>You are Claude</system-reminder>"}}}}"#
        )
        .unwrap();
        // Message with real content + system-reminder (should keep cleaned version)
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u2","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"user","content":"real question <system-reminder>noise</system-reminder>"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1); // Only the one with real content
        assert_eq!(stats.filtered, 1); // The pure system-reminder was filtered

        let results = db::search(&conn, "real", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].content.contains("system-reminder"));
    }

    #[test]
    fn test_import_no_uuid() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":"no uuid msg"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1);

        let results = db::search(&conn, "uuid", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_import_malformed_json() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, "not valid json").unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","message":{{"role":"user","content":"valid"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1);
        assert_eq!(stats.errors, 1);
    }
}
