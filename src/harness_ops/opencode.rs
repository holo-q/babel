use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent_kind::AgentKind;

use rusqlite::{Connection, OpenFlags};

use super::{AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit};

#[derive(Default)]
struct OpenCodeDiscovery {
    state_roots: BTreeSet<PathBuf>,
    missing_roots: BTreeSet<PathBuf>,
    sessions_found: usize,
    path_references_found: usize,
    edits: Vec<MigrationEdit>,
    notes: Vec<String>,
}

#[derive(Default)]
struct DbFacts {
    schema: &'static str,
    sessions: usize,
    path_refs: usize,
}

pub(super) fn plan(context: &HarnessOpsContext) -> HarnessMigrationReport {
    let discovery = discover(context);
    let state_roots = discovery.state_roots.into_iter().collect();
    let mut notes = vec![
        "storage: SQLite under XDG data root; legacy JSON under storage/{session,message,part}"
            .to_string(),
    ];
    notes.extend(discovery.notes);
    for missing in discovery.missing_roots {
        notes.push(format!("state root missing: {}", missing.display()));
    }

    HarnessMigrationReport::from_edits(
        AgentKind::OpenCode,
        AdapterReadiness::DoctorOnly,
        state_roots,
        discovery.sessions_found,
        discovery.path_references_found,
        discovery.edits,
        notes,
    )
}

fn discover(context: &HarnessOpsContext) -> OpenCodeDiscovery {
    let mut discovery = OpenCodeDiscovery::default();
    let base_dirs = opencode_base_dirs(context);
    let config_paths = opencode_config_paths(context);

    for config_path in config_paths {
        if config_path.exists() {
            discovery.state_roots.insert(config_path.clone());
            for db_path in configured_data_dirs(&config_path) {
                inspect_db_path(db_path.join("opencode.db"), &mut discovery);
            }
        }
    }

    for db_path in env_db_paths(context) {
        inspect_db_path(db_path, &mut discovery);
    }

    for base_dir in base_dirs {
        inspect_base_dir(&base_dir, &mut discovery);
    }

    discovery
}

fn inspect_base_dir(base_dir: &Path, discovery: &mut OpenCodeDiscovery) {
    if base_dir.exists() {
        discovery.state_roots.insert(base_dir.to_path_buf());
    } else {
        discovery.missing_roots.insert(base_dir.to_path_buf());
    }

    inspect_db_path(base_dir.join("opencode.db"), discovery);
    if let Ok(entries) = fs::read_dir(base_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("opencode-") && name.ends_with(".db") {
                inspect_db_path(path, discovery);
            }
        }
    }

    inspect_legacy_storage(&base_dir.join("storage"), discovery);
}

fn inspect_db_path(path: PathBuf, discovery: &mut OpenCodeDiscovery) {
    if !path.exists() {
        discovery.missing_roots.insert(path);
        return;
    }
    discovery.state_roots.insert(path.clone());

    match sqlite_facts(&path) {
        Ok(facts) => {
            discovery.sessions_found += facts.sessions;
            discovery.path_references_found += facts.path_refs;
            discovery
                .edits
                .push(MigrationEdit::preserve_session_keyed_files(
                    AgentKind::OpenCode,
                    "preserve_sqlite_database",
                    path.clone(),
                    facts.sessions,
                    facts.path_refs,
                ));
            discovery.notes.push(format!(
                "SQLite {}: schema={}, sessions={}, path-bearing row(s)={}",
                path.display(),
                facts.schema,
                facts.sessions,
                facts.path_refs
            ));
        }
        Err(err) => discovery.notes.push(format!(
            "could not inspect OpenCode SQLite {}: {err}",
            path.display()
        )),
    }
}

fn sqlite_facts(path: &Path) -> rusqlite::Result<DbFacts> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;

    if table_exists(&conn, "session")? {
        let sessions = count_rows(&conn, "session")?;
        let mut path_refs = count_nonempty_column(&conn, "session", "directory")?;
        if table_exists(&conn, "project")? {
            path_refs += count_nonempty_existing_columns(&conn, "project", &["directory", "path"])?;
        }
        if table_exists(&conn, "file")? {
            path_refs += count_nonempty_existing_columns(&conn, "file", &["path"])?;
        }
        return Ok(DbFacts {
            schema: "current(session/message/part/project)",
            sessions,
            path_refs,
        });
    }

    if table_exists(&conn, "sessions")? {
        let sessions = count_rows(&conn, "sessions")?;
        let path_refs = if table_exists(&conn, "files")? {
            count_nonempty_existing_columns(&conn, "files", &["path"])?
        } else {
            0
        };
        return Ok(DbFacts {
            schema: "casr(sessions/messages/files)",
            sessions,
            path_refs,
        });
    }

    Ok(DbFacts {
        schema: "unknown",
        sessions: 0,
        path_refs: 0,
    })
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )
}

fn count_rows(conn: &Connection, table: &str) -> rusqlite::Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as usize)
}

fn count_nonempty_existing_columns(
    conn: &Connection,
    table: &str,
    columns: &[&str],
) -> rusqlite::Result<usize> {
    let existing = table_columns(conn, table)?;
    let mut count = 0;
    for column in columns {
        if existing.contains(*column) {
            count += count_nonempty_column(conn, table, column)?;
        }
    }
    Ok(count)
}

fn count_nonempty_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} IS NOT NULL AND {column} != ''");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as usize)
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<BTreeSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = BTreeSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn inspect_legacy_storage(storage_dir: &Path, discovery: &mut OpenCodeDiscovery) {
    if !storage_dir.exists() {
        discovery.missing_roots.insert(storage_dir.to_path_buf());
        return;
    }

    let session_root = storage_dir.join("session");
    let message_root = storage_dir.join("message");
    let part_root = storage_dir.join("part");
    discovery.state_roots.insert(storage_dir.to_path_buf());
    for root in [&session_root, &message_root, &part_root] {
        if root.exists() {
            discovery.state_roots.insert(root.to_path_buf());
        }
    }

    let mut sessions = 0;
    let mut directory_refs = 0;
    if let Ok(project_dirs) = fs::read_dir(&session_root) {
        for project_dir in project_dirs.flatten() {
            let Ok(metadata) = project_dir.metadata() else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            let Ok(session_files) = fs::read_dir(project_dir.path()) else {
                continue;
            };
            for session_file in session_files.flatten() {
                let path = session_file.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                sessions += 1;
                if json_has_string_at(&path, &["directory"]) {
                    directory_refs += 1;
                }
            }
        }
    }

    discovery.sessions_found += sessions;
    discovery.path_references_found += directory_refs;
    if sessions > 0 {
        discovery
            .edits
            .push(MigrationEdit::preserve_session_keyed_files(
                AgentKind::OpenCode,
                "preserve_legacy_json_storage",
                storage_dir.to_path_buf(),
                sessions,
                directory_refs,
            ));
    }
    discovery.notes.push(format!(
        "legacy JSON storage {}: sessions={}, directory-bearing session file(s)={}",
        storage_dir.display(),
        sessions,
        directory_refs
    ));
}

fn json_has_string_at(path: &Path, keys: &[&str]) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };

    let mut current = &value;
    for key in keys {
        let Some(next) = current.get(*key) else {
            return false;
        };
        current = next;
    }
    current
        .as_str()
        .is_some_and(|value| !value.trim().is_empty())
}

fn configured_data_dirs(config_path: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .pointer("/data/directory")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

fn opencode_base_dirs(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            let path = PathBuf::from(xdg).join("opencode");
            if env_path_applies(context, &path) {
                dirs.push(path);
            }
        }
    }
    dirs.push(context.home.join(".local/share/opencode"));
    dirs.push(context.home.join(".opencode"));
    dedup_paths(dirs)
}

fn opencode_config_paths(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut paths = vec![
        context.home.join(".opencode.json"),
        context.home.join(".config/opencode/.opencode.json"),
        context.home.join(".config/opencode/opencode.json"),
    ];
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            let legacy = PathBuf::from(&xdg).join("opencode/.opencode.json");
            let current = PathBuf::from(&xdg).join("opencode/opencode.json");
            if env_path_applies(context, &legacy) {
                paths.push(legacy);
            }
            if env_path_applies(context, &current) {
                paths.push(current);
            }
        }
    }
    dedup_paths(paths)
}

fn env_db_paths(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for key in ["OPENCODE_DB", "OPENCODE_DB_PATH"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                let path = PathBuf::from(value);
                if env_path_applies(context, &path) {
                    paths.push(path);
                }
            }
        }
    }
    if let Ok(value) = std::env::var("OPENCODE_HOME") {
        if !value.trim().is_empty() {
            let path = PathBuf::from(value);
            if !env_path_applies(context, &path) {
                return dedup_paths(paths);
            }
            if path.extension().is_some_and(|ext| ext == "db") {
                paths.push(path);
            } else {
                paths.push(path.join("opencode.db"));
            }
        }
    }
    dedup_paths(paths)
}

fn env_path_applies(context: &HarnessOpsContext, path: &Path) -> bool {
    if path.starts_with(&context.home) {
        return true;
    }
    dirs::home_dir().is_some_and(|home| home == context.home)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_reports_sqlite_session_and_path_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessOpsContext::from_home(tmp.path().to_path_buf());
        let base = tmp.path().join(".local/share/opencode");
        fs::create_dir_all(&base).unwrap();
        let db_path = base.join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT,
                directory TEXT,
                title TEXT,
                version TEXT,
                time_created INTEGER,
                time_updated INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                time_created INTEGER,
                data TEXT
            );
            INSERT INTO session VALUES
                ('ses_1', 'proj', '/home/user/project', 'one', '1.2.0', 1, 2),
                ('ses_2', 'proj', '', 'two', '1.2.0', 3, 4);
            "#,
        )
        .unwrap();
        drop(conn);

        let report = plan(&context);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert!(report.state_roots.contains(&db_path));
        assert_eq!(report.sessions_found, 2);
        assert_eq!(report.path_references_found, 1);
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "preserve_sqlite_database" && !op.apply_ready));
    }

    #[test]
    fn opencode_reports_legacy_json_storage_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessOpsContext::from_home(tmp.path().to_path_buf());
        let storage = tmp.path().join(".local/share/opencode/storage");
        let session_dir = storage.join("session/proj");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(storage.join("message/ses_1")).unwrap();
        fs::create_dir_all(storage.join("part/msg_1")).unwrap();
        fs::write(
            session_dir.join("ses_1.json"),
            r#"{"id":"ses_1","directory":"/home/user/project","time":{"created":1}}"#,
        )
        .unwrap();

        let report = plan(&context);
        assert!(report.state_roots.contains(&storage));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 1);
        assert!(report
            .edits
            .iter()
            .any(|edit| edit.action == "preserve_legacy_json_storage" && !edit.apply_ready));
    }
}
