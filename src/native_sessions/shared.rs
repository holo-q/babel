use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::session_row;
use crate::AgentKind;

/// A session discovered from native harness storage.
///
/// This is a read-through view: no cache, no overlay DB. Harness modules own
/// provider storage details; callers only receive this normalized row shape.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeSession {
    pub agent_kind: AgentKind,
    pub native_id: String,
    pub project_path: Option<String>,
    pub display_name: Option<String>,
    pub last_prompt: Option<String>,
    pub turn_count: u32,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub interactive: bool,
    /// Every user prompt was a slash command (/model, /usage, etc.).
    pub command_only: bool,
    /// display_name is a proper title (thread name, /rename) not a prompt fallback.
    pub has_title: bool,
}

/// Filter flags shared by ls-sessions, resume, and session hide/unhide.
#[derive(Debug, Default)]
pub struct SessionFilters {
    pub sub: bool,
    pub oneshot: bool,
    pub commands: bool,
    pub all: bool,
}

#[derive(Default)]
pub(crate) struct HarnessTranscriptFacts {
    pub native_id: Option<String>,
    pub project_path: Option<String>,
    pub display_name: Option<String>,
    pub first_user: Option<String>,
    pub last_user: Option<String>,
    pub user_turns: u32,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub line_count: u32,
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub(crate) fn is_slash_command(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('/') && !trimmed.contains('\n')
}

pub(crate) fn home_path(parts: &[&str]) -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

pub(crate) fn xdg_data_path(parts: &[&str]) -> Option<PathBuf> {
    let mut path = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_path(&[".local", "share"]))?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

pub(crate) fn push_existing_path(paths: &mut Vec<PathBuf>, path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };
    if path.exists() && !paths.iter().any(|known| known == &path) {
        paths.push(path);
    }
}

pub(crate) fn modified_secs(path: &Path) -> i64 {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn read_json(path: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) fn json_str<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn json_path_str<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_str().filter(|text| !text.trim().is_empty())
}

pub(crate) fn json_epoch_secs(value: &serde_json::Value) -> Option<i64> {
    if let Some(num) = value.as_i64() {
        return Some(if num > 1_000_000_000_000 {
            num / 1000
        } else {
            num
        });
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(num) = text.parse::<i64>() {
        return Some(if num > 1_000_000_000_000 {
            num / 1000
        } else {
            num
        });
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp())
}

pub(crate) fn json_time(value: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(ts) = value.get(*key).and_then(json_epoch_secs) {
            return Some(ts);
        }
    }
    None
}

pub(crate) fn clean_session_text(text: &str) -> Option<String> {
    let cleaned = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(session_row::sanitize_display(&cleaned, 120))
    }
}

fn text_from_json(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => clean_session_text(text),
        serde_json::Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(text_from_json)
                .collect::<Vec<_>>()
                .join("\n");
            clean_session_text(&text)
        }
        serde_json::Value::Object(_) => {
            if let Some(text) = json_str(value, &["text", "content", "body", "delta", "message"]) {
                return clean_session_text(text);
            }
            for path in [
                &["message", "content"][..],
                &["message", "text"][..],
                &["data", "content"][..],
                &["params", "content"][..],
            ] {
                if let Some(nested) = path.iter().try_fold(value, |cursor, key| cursor.get(*key)) {
                    if let Some(text) = text_from_json(nested) {
                        return Some(text);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn json_role(value: &serde_json::Value) -> Option<&str> {
    json_str(value, &["role", "author", "speaker"])
        .or_else(|| json_path_str(value, &["message", "role"]))
        .or_else(|| json_str(value, &["type", "event", "kind"]))
}

pub(crate) fn update_facts_from_record(
    facts: &mut HarnessTranscriptFacts,
    value: &serde_json::Value,
) {
    if facts.native_id.is_none() {
        facts.native_id = json_str(
            value,
            &[
                "sessionId",
                "session_id",
                "sessionID",
                "conversationId",
                "conversation_id",
                "threadId",
                "thread_id",
                "taskId",
                "task_id",
                "id",
            ],
        )
        .map(str::to_string);
    }
    if facts.project_path.is_none() {
        facts.project_path = json_str(
            value,
            &[
                "cwd",
                "workspace",
                "workspacePath",
                "workspace_path",
                "directory",
                "projectPath",
                "project_path",
            ],
        )
        .map(str::to_string);
    }
    if facts.display_name.is_none() {
        facts.display_name = json_str(
            value,
            &["title", "summary", "name", "sessionTitle", "session_title"],
        )
        .and_then(clean_session_text);
    }
    if let Some(ts) = json_time(
        value,
        &[
            "timestamp",
            "ts",
            "createdAt",
            "created_at",
            "updatedAt",
            "updated_at",
            "time_updated",
            "timeCreated",
            "time_created",
        ],
    ) {
        if facts.first_seen_at == 0 || ts < facts.first_seen_at {
            facts.first_seen_at = ts;
        }
        facts.last_seen_at = facts.last_seen_at.max(ts);
    }

    let role = json_role(value).unwrap_or_default().to_ascii_lowercase();
    let userish = role == "user"
        || role == "human"
        || role == "prompt"
        || role == "user_message"
        || role == "user_message_chunk"
        || value.get("say").and_then(|v| v.as_str()) == Some("user_feedback");
    if userish {
        if let Some(text) = text_from_json(value) {
            if !is_slash_command(&text) {
                if facts.first_user.is_none() {
                    facts.first_user = Some(text.clone());
                }
                facts.last_user = Some(text);
                facts.user_turns += 1;
            }
        }
    } else if facts.first_user.is_none() {
        facts.first_user = text_from_json(value);
    }
}

pub(crate) fn jsonl_facts(path: &Path) -> HarnessTranscriptFacts {
    use std::io::{BufRead, BufReader};

    let mut facts = HarnessTranscriptFacts::default();
    let Ok(file) = std::fs::File::open(path) else {
        return facts;
    };
    for line in BufReader::new(file).lines().flatten() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        facts.line_count += 1;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            update_facts_from_record(&mut facts, &value);
        }
    }
    facts
}

pub(crate) fn json_array_facts(path: &Path) -> HarnessTranscriptFacts {
    let mut facts = HarnessTranscriptFacts::default();
    let Some(value) = read_json(path) else {
        return facts;
    };
    match value {
        serde_json::Value::Array(items) => {
            for item in &items {
                facts.line_count += 1;
                update_facts_from_record(&mut facts, item);
            }
        }
        serde_json::Value::Object(_) => update_facts_from_record(&mut facts, &value),
        _ => {}
    }
    facts
}

pub(crate) fn file_stem_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

pub(crate) fn session_from_facts(
    kind: AgentKind,
    fallback_id: Option<String>,
    fallback_project: Option<String>,
    fallback_title: Option<String>,
    fallback_seen: i64,
    facts: HarnessTranscriptFacts,
) -> Option<NativeSession> {
    let native_id = facts.native_id.or(fallback_id)?;
    let display_name = fallback_title
        .or(facts.display_name)
        .or_else(|| facts.first_user.clone());
    Some(NativeSession {
        agent_kind: kind,
        native_id,
        project_path: facts.project_path.or(fallback_project),
        display_name,
        last_prompt: if facts.user_turns > 1 {
            facts.last_user
        } else {
            None
        },
        turn_count: facts.user_turns,
        created_at: if facts.first_seen_at > 0 {
            facts.first_seen_at
        } else {
            fallback_seen
        },
        last_seen_at: facts.last_seen_at.max(fallback_seen),
        interactive: facts.user_turns > 0 || facts.line_count > 0,
        command_only: false,
        has_title: false,
    })
}

pub(crate) fn visit_files(
    root: &Path,
    max_depth: usize,
    accept: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) {
    if max_depth == 0 || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_files(&path, max_depth - 1, accept, out);
        } else if accept(&path) {
            out.push(path);
        }
    }
}

pub(crate) fn all_jsonl_sessions(
    kind: AgentKind,
    roots: Vec<PathBuf>,
    max_depth: usize,
) -> Vec<NativeSession> {
    let mut files = Vec::new();
    for root in roots {
        visit_files(
            &root,
            max_depth,
            &|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"),
            &mut files,
        );
    }
    files
        .into_iter()
        .filter_map(|path| {
            let facts = jsonl_facts(&path);
            session_from_facts(
                kind,
                file_stem_id(&path),
                None,
                None,
                modified_secs(&path),
                facts,
            )
        })
        .collect()
}

fn sqlite_open_read_only(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn sqlite_table_columns(
    conn: &rusqlite::Connection,
    table: &str,
) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows.flatten().collect())
}

fn pick_column(columns: &HashSet<String>, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| columns.contains(**candidate))
        .map(|candidate| (*candidate).to_string())
}

fn query_sqlite_session_table(
    kind: AgentKind,
    db_path: &Path,
    table: &str,
) -> Result<Vec<NativeSession>> {
    let conn = sqlite_open_read_only(db_path)?;
    let columns = sqlite_table_columns(&conn, table)?;
    let Some(id_col) = pick_column(
        &columns,
        &[
            "id",
            "session_id",
            "sessionId",
            "conversation_id",
            "conversationId",
            "thread_id",
        ],
    ) else {
        return Ok(Vec::new());
    };
    let cwd_col = pick_column(
        &columns,
        &[
            "cwd",
            "directory",
            "workspace",
            "workspace_path",
            "workspacePath",
            "path",
        ],
    );
    let title_col = pick_column(&columns, &["title", "summary", "name", "slug"]);
    let updated_col = pick_column(
        &columns,
        &[
            "updated_at",
            "updatedAt",
            "time_updated",
            "timeUpdated",
            "mtime",
        ],
    );
    let created_col = pick_column(
        &columns,
        &[
            "created_at",
            "createdAt",
            "time_created",
            "timeCreated",
            "ctime",
        ],
    );
    let sql = format!(
        "SELECT {id_col}, {}, {}, {}, {} FROM {table}",
        cwd_col.clone().unwrap_or_else(|| "NULL".to_string()),
        title_col.clone().unwrap_or_else(|| "NULL".to_string()),
        updated_col
            .as_ref()
            .map(|col| format!("CAST({col} AS TEXT)"))
            .unwrap_or_else(|| "NULL".to_string()),
        created_col
            .as_ref()
            .map(|col| format!("CAST({col} AS TEXT)"))
            .unwrap_or_else(|| "NULL".to_string())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3).ok().flatten(),
            row.get::<_, Option<String>>(4).ok().flatten(),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (native_id, project_path, display_name, updated, created) = row;
        if native_id.is_empty() {
            continue;
        }
        let last_seen_at = updated
            .as_ref()
            .and_then(|text| json_epoch_secs(&serde_json::Value::String(text.clone())))
            .or_else(|| {
                created
                    .as_ref()
                    .and_then(|text| json_epoch_secs(&serde_json::Value::String(text.clone())))
            })
            .unwrap_or_else(|| modified_secs(db_path));
        let created_at = created
            .as_ref()
            .and_then(|text| json_epoch_secs(&serde_json::Value::String(text.clone())))
            .unwrap_or(last_seen_at);
        out.push(NativeSession {
            agent_kind: kind,
            native_id,
            project_path,
            display_name: display_name.and_then(|text| clean_session_text(&text)),
            last_prompt: None,
            turn_count: 0,
            created_at,
            last_seen_at,
            interactive: true,
            command_only: false,
            has_title: title_col.is_some(),
        });
    }
    Ok(out)
}

pub(crate) fn query_sqlite_sessions(
    kind: AgentKind,
    db_path: &Path,
    tables: &[&str],
) -> Vec<NativeSession> {
    let Ok(conn) = sqlite_open_read_only(db_path) else {
        return Vec::new();
    };
    let table_names: HashSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    drop(conn);

    for table in tables {
        if table_names.contains(*table) {
            if let Ok(rows) = query_sqlite_session_table(kind, db_path, table) {
                if !rows.is_empty() {
                    return rows;
                }
            }
        }
    }
    Vec::new()
}

pub(crate) fn scan_cline_family(
    kind: AgentKind,
    env_names: &[&str],
    extension_ids: &[&str],
) -> Result<Vec<NativeSession>> {
    let mut roots = Vec::new();
    for name in env_names {
        push_existing_path(&mut roots, env_path(name));
    }
    for extension_id in extension_ids {
        push_existing_path(
            &mut roots,
            home_path(&[
                ".config",
                "Code",
                "User",
                "globalStorage",
                extension_id,
                "tasks",
            ]),
        );
        push_existing_path(
            &mut roots,
            home_path(&[
                ".config",
                "Cursor",
                "User",
                "globalStorage",
                extension_id,
                "tasks",
            ]),
        );
    }

    let mut files = Vec::new();
    for root in roots {
        visit_files(
            &root,
            3,
            &|path| path.file_name().and_then(|name| name.to_str()) == Some("ui_messages.json"),
            &mut files,
        );
    }
    let mut out = Vec::new();
    for path in files {
        let task_id = path
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .map(str::to_string);
        if let Some(session) = session_from_facts(
            kind,
            task_id,
            None,
            None,
            modified_secs(&path),
            json_array_facts(&path),
        ) {
            out.push(session);
        }
    }
    Ok(out)
}
