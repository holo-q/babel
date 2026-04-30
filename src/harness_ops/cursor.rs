use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit};

#[derive(Default)]
struct CursorDiscovery {
    state_roots: Vec<PathBuf>,
    sqlite_dbs: Vec<CursorDbDiscovery>,
    workspaces: Vec<CursorWorkspace>,
    legacy_project_files: usize,
    legacy_chat_files: usize,
    sqlite_open_failures: usize,
    sessions_found: usize,
    path_references_found: usize,
}

struct CursorDbDiscovery {
    path: PathBuf,
    composer_rows: usize,
    bubble_rows: usize,
    legacy_rows: usize,
    path_ref_rows: usize,
}

struct CursorWorkspace {
    storage_dir: PathBuf,
    state_db: PathBuf,
    folder: Option<String>,
}

pub(super) fn plan(context: &HarnessOpsContext) -> HarnessMigrationReport {
    let discovery = discover(context);
    let mut edits = Vec::new();

    for db in &discovery.sqlite_dbs {
        let session_count = db.composer_rows.max(db.legacy_rows);
        if session_count == 0 && db.path_ref_rows == 0 {
            continue;
        }
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::Cursor,
            "preserve_cursor_sqlite_state",
            db.path.clone(),
            session_count,
            db.path_ref_rows,
        ));
    }

    for workspace in &discovery.workspaces {
        if !workspace.state_db.exists() {
            continue;
        }
        let folder = workspace
            .folder
            .as_deref()
            .unwrap_or("workspace folder unknown");
        edits.push(MigrationEdit::preserve_project_local_history(
            AgentKind::Cursor,
            workspace.state_db.display().to_string(),
            format!("workspaceStorage maps to {folder}; preserve workspace.json with state.vscdb"),
        ));
    }

    if discovery.legacy_project_files > 0 {
        edits.push(MigrationEdit::preserve_project_local_history(
            AgentKind::Cursor,
            context.home.join(".cursor/projects").display().to_string(),
            format!(
                "legacy ~/.cursor/projects contains {} file(s); preserve as opaque project history",
                discovery.legacy_project_files
            ),
        ));
    }
    if discovery.legacy_chat_files > 0 {
        edits.push(MigrationEdit::preserve_project_local_history(
            AgentKind::Cursor,
            context.home.join(".cursor/chats").display().to_string(),
            format!(
                "legacy ~/.cursor/chats contains {} file(s); preserve as opaque chat history",
                discovery.legacy_chat_files
            ),
        ));
    }

    let mut notes = vec![
        "Cursor storage is SQLite-backed; Babel opens databases read-only and does not add a new DB dependency beyond the repository's existing rusqlite dependency.".to_string(),
        "Modern Cursor Agent sessions live in cursorDiskKV: composerData:<id> rows plus bubbleId:<composerId>:<bubbleId> rows.".to_string(),
        "Legacy chat data can appear in ItemTable keys such as composer.composerData and workbench.panel.aichat.view.aichat.chatdata.".to_string(),
        "workspaceStorage/<hash>/workspace.json is the concrete hash-to-folder map; state.vscdb beside it is project-local history.".to_string(),
        "Close Cursor before any future write/copy operation; Cursor may hold state.vscdb locks and recovery references back up databases/images before replacement.".to_string(),
        "No Cursor rewrite operation is apply-ready here: the adapter is not passed source/destination paths by the parent planner, and SQLite JSON mutation needs a narrower transaction contract.".to_string(),
    ];

    if discovery.state_roots.is_empty() {
        notes.push("no Cursor state roots detected".to_string());
    }
    if discovery.sqlite_open_failures > 0 {
        notes.push(format!(
            "failed to open {} Cursor SQLite database(s) read-only; close Cursor or inspect permissions",
            discovery.sqlite_open_failures
        ));
    }
    if !discovery.workspaces.is_empty() {
        let examples = discovery
            .workspaces
            .iter()
            .filter_map(|workspace| {
                workspace
                    .folder
                    .as_ref()
                    .map(|folder| format!("{} -> {folder}", workspace.storage_dir.display()))
            })
            .take(3)
            .collect::<Vec<_>>();
        if !examples.is_empty() {
            let suffix = if discovery.workspaces.len() > examples.len() {
                ", ..."
            } else {
                ""
            };
            notes.push(format!(
                "workspaceStorage folder refs: {}{suffix}",
                examples.join(", ")
            ));
        }
    }
    if !discovery.sqlite_dbs.is_empty() {
        let db_summary = discovery
            .sqlite_dbs
            .iter()
            .take(3)
            .map(|db| {
                format!(
                    "{} (composer={}, bubble={}, legacy={}, path_ref_rows={})",
                    db.path.display(),
                    db.composer_rows,
                    db.bubble_rows,
                    db.legacy_rows,
                    db.path_ref_rows
                )
            })
            .collect::<Vec<_>>();
        let suffix = if discovery.sqlite_dbs.len() > db_summary.len() {
            ", ..."
        } else {
            ""
        };
        notes.push(format!(
            "Cursor SQLite discoveries: {}{suffix}",
            db_summary.join(", ")
        ));
    }

    HarnessMigrationReport::from_edits(
        AgentKind::Cursor,
        AdapterReadiness::DoctorOnly,
        discovery.state_roots,
        discovery.sessions_found,
        discovery.path_references_found,
        edits,
        notes,
    )
}

fn discover(context: &HarnessOpsContext) -> CursorDiscovery {
    let mut discovery = CursorDiscovery::default();
    let global_db = context
        .home
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    let workspace_storage = context.home.join(".config/Cursor/User/workspaceStorage");
    let legacy_projects = context.home.join(".cursor/projects");
    let legacy_chats = context.home.join(".cursor/chats");

    for root in context.cursor_roots() {
        if root.exists() {
            discovery.state_roots.push(root);
        }
    }

    if legacy_projects.exists() {
        discovery.legacy_project_files = count_regular_files(&legacy_projects);
    }
    if legacy_chats.exists() {
        discovery.legacy_chat_files = count_regular_files(&legacy_chats);
    }

    let mut db_paths = Vec::new();
    if global_db.is_file() {
        db_paths.push(global_db);
    }

    if workspace_storage.is_dir() {
        if let Ok(entries) = fs::read_dir(&workspace_storage) {
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let storage_dir = entry.path();
                let state_db = storage_dir.join("state.vscdb");
                let workspace = CursorWorkspace {
                    folder: read_workspace_folder(&storage_dir.join("workspace.json")),
                    storage_dir,
                    state_db: state_db.clone(),
                };
                if state_db.is_file() {
                    db_paths.push(state_db);
                }
                discovery.workspaces.push(workspace);
            }
        }
    }

    db_paths.sort();
    db_paths.dedup();
    for db_path in db_paths {
        match inspect_cursor_db(&db_path) {
            Ok(db) => {
                discovery.sessions_found += db.composer_rows.max(db.legacy_rows);
                discovery.path_references_found += db.path_ref_rows;
                discovery.sqlite_dbs.push(db);
            }
            Err(()) => discovery.sqlite_open_failures += 1,
        }
    }

    discovery.state_roots.sort();
    discovery.state_roots.dedup();
    discovery
}

fn inspect_cursor_db(path: &Path) -> Result<CursorDbDiscovery, ()> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ())?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(250));

    let mut db = CursorDbDiscovery {
        path: path.to_path_buf(),
        composer_rows: 0,
        bubble_rows: 0,
        legacy_rows: 0,
        path_ref_rows: 0,
    };

    if table_exists(&conn, "cursorDiskKV") {
        db.composer_rows =
            count_table_key_like(&conn, "cursorDiskKV", "composerData:%").unwrap_or(0);
        db.bubble_rows = count_table_key_like(&conn, "cursorDiskKV", "bubbleId:%").unwrap_or(0);
        db.path_ref_rows += count_path_bearing_rows(&conn, "cursorDiskKV").unwrap_or(0);
    }

    if table_exists(&conn, "ItemTable") {
        db.legacy_rows = count_itemtable_sessions(&conn).unwrap_or(0);
        db.path_ref_rows += count_path_bearing_rows(&conn, "ItemTable").unwrap_or(0);
    }

    Ok(db)
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
        .and_then(|mut stmt| stmt.exists([table]))
        .unwrap_or(false)
}

fn count_table_key_like(conn: &Connection, table: &str, pattern: &str) -> rusqlite::Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE key LIKE ?1");
    conn.query_row(&sql, [pattern], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as usize)
}

fn count_itemtable_sessions(conn: &Connection) -> rusqlite::Result<usize> {
    let rows = query_values_for_keys(
        conn,
        "ItemTable",
        &[
            "composer.composerData",
            "workbench.panel.aichat.view.aichat.chatdata",
            "workbench.panel.chat.view.chat.chatdata",
        ],
    )?;

    let mut count = 0;
    for value in rows {
        let Ok(json) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        count += match json {
            Value::Array(items) => items.len(),
            Value::Object(_) => 1,
            _ => 0,
        };
    }
    Ok(count)
}

fn query_values_for_keys(
    conn: &Connection,
    table: &str,
    keys: &[&str],
) -> rusqlite::Result<Vec<String>> {
    let mut values = Vec::new();
    let sql = format!("SELECT value FROM {table} WHERE key = ?1");
    let mut stmt = conn.prepare(&sql)?;

    for key in keys {
        let rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
        for row in rows.flatten() {
            values.push(row);
        }
    }
    Ok(values)
}

fn count_path_bearing_rows(conn: &Connection, table: &str) -> rusqlite::Result<usize> {
    let sql = format!("SELECT value FROM {table}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows.flatten() {
        if value_has_path_reference(&row) {
            count += 1;
        }
    }
    Ok(count)
}

fn value_has_path_reference(value: &str) -> bool {
    if value.contains("file://")
        || value.contains("\"workspaceUri\"")
        || value.contains("\"projectDir\"")
        || value.contains("\"workspacePath\"")
    {
        return true;
    }

    let Ok(json) = serde_json::from_str::<Value>(value) else {
        return false;
    };
    json_contains_path_string(&json)
}

fn json_contains_path_string(value: &Value) -> bool {
    match value {
        Value::String(text) => looks_like_absolute_path(text) || contains_embedded_path(text),
        Value::Array(items) => items.iter().any(json_contains_path_string),
        Value::Object(map) => map.values().any(json_contains_path_string),
        _ => false,
    }
}

fn contains_embedded_path(text: &str) -> bool {
    text.contains("file:///") || text.contains("\"/")
}

fn looks_like_absolute_path(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with("file:///")
        || (text.len() > 3
            && text.as_bytes()[1] == b':'
            && matches!(text.as_bytes()[2], b'/' | b'\\'))
}

fn read_workspace_folder(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<Value>(&content).ok()?;
    value
        .get("folder")
        .and_then(Value::as_str)
        .map(decode_file_uri_lossy)
}

fn decode_file_uri_lossy(value: &str) -> String {
    value.strip_prefix("file://").unwrap_or(value).to_string()
}

fn count_regular_files(root: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        } else if metadata.is_file() {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rusqlite::params;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn create_cursor_db(path: &Path) -> Connection {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn cursor_doctor_reports_sqlite_and_workspace_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ctx = HarnessOpsContext::from_home(home.to_path_buf());

        let global_db = home.join(".config/Cursor/User/globalStorage/state.vscdb");
        let conn = create_cursor_db(&global_db);
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            params![
                "composerData:comp-1",
                r#"{"composerId":"comp-1","workspaceUri":"file:///workspace/old"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            params![
                "bubbleId:comp-1:bubble-1",
                r#"{"text":"hello","toolFormerData":{"params":"{\"path\":\"/workspace/old/src/lib.rs\"}"}}"#
            ],
        )
        .unwrap();
        drop(conn);

        let workspace_dir = home.join(".config/Cursor/User/workspaceStorage/ws-1");
        write_file(
            &workspace_dir.join("workspace.json"),
            r#"{"folder":"file:///workspace/old"}"#,
        );
        let workspace_db = workspace_dir.join("state.vscdb");
        let conn = create_cursor_db(&workspace_db);
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            params![
                "composer.composerData",
                r#"[{"composerId":"legacy-1","projectDir":"/workspace/old"}]"#
            ],
        )
        .unwrap();
        drop(conn);

        let report = plan(&ctx);

        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 2);
        assert_eq!(report.path_references_found, 3);
        assert!(report
            .operations
            .iter()
            .all(|operation| !operation.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|operation| operation.action == "preserve_cursor_sqlite_state"));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("workspaceStorage folder refs")
                && note.contains("/workspace/old")));
    }
}
