use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::agent_kind::AgentKind;

use super::{
    open_sqlite_read_only, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext,
    MigrationEdit,
};

const CRUSH_DB_FILE: &str = "crush.db";
const CRUSH_DATA_DIR: &str = ".crush";
const PROJECTS_INDEX: &str = "projects.json";

#[derive(Debug, Clone)]
struct CrushDbCandidate {
    path: PathBuf,
    cwd: Option<PathBuf>,
    source: &'static str,
}

#[derive(Debug, Default)]
struct CrushDiscovery {
    candidates: Vec<CrushDbCandidate>,
    readable_dbs: usize,
    sessions_found: usize,
    project_scoped_sessions: usize,
    child_or_task_sessions: usize,
    path_references_found: usize,
    projects_index_refs: usize,
    sqlite_ref_cells: usize,
    schema_notes: Vec<String>,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, needles)?;
    let mut state_roots = discovery
        .candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    state_roots.extend(
        crush_global_state_roots(context)
            .into_iter()
            .filter(|path| path.exists()),
    );
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    for candidate in discovery
        .candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
    {
        let detail = match &candidate.cwd {
            Some(cwd) if path_scope_matches(cwd, old_path) => format!(
                "project-local SQLite DB follows the project move; preserve sessions.id rows and messages/files tables from {}",
                candidate.source
            ),
            Some(cwd) => format!(
                "SQLite DB is associated with {}; inspect because it was discovered from {}",
                cwd.display(),
                candidate.source
            ),
            None => format!(
                "SQLite DB discovered from {}; sessions.id is the durable storage identity",
                candidate.source
            ),
        };
        edits.push(MigrationEdit::preserve_project_local_history(
            AgentKind::Crush,
            candidate.path.display().to_string(),
            detail,
        ));
    }

    if discovery.projects_index_refs > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Crush,
            "rewrite_project_index_refs",
            crush_projects_index(context).display().to_string(),
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.projects_index_refs,
        ));
    }

    if discovery.sqlite_ref_cells > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Crush,
            "rewrite_sqlite_path_refs",
            "Crush SQLite text columns in sessions/messages/files",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.sqlite_ref_cells,
        ));
    }

    let mut notes = vec![
        "storage: SQLite crush.db with sessions, messages, and files tables".to_string(),
        "project-local database candidate: <workspace>/.crush/crush.db".to_string(),
    ];
    notes.extend(discovery.schema_notes);
    if discovery.candidates.is_empty() {
        notes.push(
            "no Crush database candidates found for this source path or known global roots"
                .to_string(),
        );
    }
    if discovery.child_or_task_sessions > 0 {
        notes.push(format!(
            "detected {} child/helper/task-like session row(s); preserve parent_session_id/tool-call-derived identity when migrating",
            discovery.child_or_task_sessions
        ));
    }

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Crush,
        AdapterReadiness::DoctorOnly,
        state_roots,
        discovery
            .project_scoped_sessions
            .max(discovery.sessions_found),
        discovery.path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    needles: &[String],
) -> Result<CrushDiscovery> {
    let mut discovery = CrushDiscovery {
        candidates: crush_db_candidates(context, old_path)?,
        ..Default::default()
    };

    let projects_index = crush_projects_index(context);
    discovery.projects_index_refs = count_text_file_refs(&projects_index, needles)?;
    discovery.path_references_found += discovery.projects_index_refs;

    for candidate in &discovery.candidates {
        if !candidate.path.exists() {
            discovery
                .schema_notes
                .push(format!("state root missing: {}", candidate.path.display()));
            continue;
        }
        let Ok(summary) = inspect_crush_db(&candidate.path, needles) else {
            discovery.schema_notes.push(format!(
                "unreadable or non-Crush SQLite database: {}",
                candidate.path.display()
            ));
            continue;
        };

        discovery.readable_dbs += 1;
        discovery.sessions_found += summary.sessions;
        discovery.child_or_task_sessions += summary.child_or_task_sessions;
        discovery.sqlite_ref_cells += summary.path_ref_cells;
        discovery.path_references_found += summary.path_ref_cells;
        if candidate
            .cwd
            .as_deref()
            .is_some_and(|cwd| path_scope_matches(cwd, old_path))
        {
            discovery.project_scoped_sessions += summary.sessions;
        }
        discovery.schema_notes.extend(summary.notes);
    }

    Ok(discovery)
}

#[derive(Default)]
struct CrushDbSummary {
    sessions: usize,
    child_or_task_sessions: usize,
    path_ref_cells: usize,
    notes: Vec<String>,
}

fn inspect_crush_db(path: &Path, needles: &[String]) -> Result<CrushDbSummary> {
    let conn = open_sqlite_read_only(path)
        .with_context(|| format!("open Crush DB read-only: {}", path.display()))?;

    let tables = table_names(&conn)?;
    if !tables.contains("sessions") || !tables.contains("messages") {
        anyhow::bail!("missing required Crush sessions/messages tables");
    }

    let session_columns = table_columns(&conn, "sessions")?;
    let mut summary = CrushDbSummary {
        sessions: query_count(&conn, "SELECT COUNT(*) FROM sessions")?,
        ..Default::default()
    };

    if session_columns.contains("parent_session_id") {
        summary.child_or_task_sessions = query_count(
            &conn,
            "SELECT COUNT(*) FROM sessions WHERE parent_session_id IS NOT NULL AND parent_session_id != ''",
        )?;
    } else {
        summary.child_or_task_sessions = query_count(
            &conn,
            "SELECT COUNT(*) FROM sessions WHERE id LIKE 'title-%'",
        )?;
    }

    for table in ["sessions", "messages", "files"] {
        if tables.contains(table) {
            summary.path_ref_cells += count_sqlite_text_refs(&conn, table, needles)?;
        }
    }

    summary.notes.push(format!(
        "{}: {} session row(s); {} source path text cell(s)",
        path.display(),
        summary.sessions,
        summary.path_ref_cells
    ));
    Ok(summary)
}

fn crush_db_candidates(
    context: &HarnessOpsContext,
    old_path: &Path,
) -> Result<Vec<CrushDbCandidate>> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();

    add_candidate(
        &mut candidates,
        &mut seen,
        old_path.join(CRUSH_DATA_DIR).join(CRUSH_DB_FILE),
        Some(old_path.to_path_buf()),
        "source_path_project_local",
    );

    for ancestor in old_path.ancestors().skip(1) {
        add_candidate(
            &mut candidates,
            &mut seen,
            ancestor.join(CRUSH_DATA_DIR).join(CRUSH_DB_FILE),
            Some(ancestor.to_path_buf()),
            "ancestor_project_local",
        );
    }

    add_candidate(
        &mut candidates,
        &mut seen,
        context.home.join(CRUSH_DATA_DIR).join(CRUSH_DB_FILE),
        None,
        "legacy_home_default",
    );

    for (path, cwd) in project_index_candidates(context)? {
        add_candidate(
            &mut candidates,
            &mut seen,
            path,
            cwd,
            "global_projects_index",
        );
    }

    Ok(candidates)
}

fn add_candidate(
    candidates: &mut Vec<CrushDbCandidate>,
    seen: &mut BTreeSet<PathBuf>,
    path: PathBuf,
    cwd: Option<PathBuf>,
    source: &'static str,
) {
    if seen.insert(path.clone()) {
        candidates.push(CrushDbCandidate { path, cwd, source });
    }
}

fn project_index_candidates(
    context: &HarnessOpsContext,
) -> Result<Vec<(PathBuf, Option<PathBuf>)>> {
    let path = crush_projects_index(context);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(&path)?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(Vec::new());
    };
    let Some(projects) = value.get("projects").and_then(|value| value.as_array()) else {
        return Ok(Vec::new());
    };

    let mut candidates = Vec::new();
    for project in projects {
        let data_dir = project
            .get("data_dir")
            .and_then(|value| value.as_str())
            .map(PathBuf::from);
        let project_path = project
            .get("path")
            .and_then(|value| value.as_str())
            .map(PathBuf::from);
        if let Some(data_dir) = data_dir {
            candidates.push((data_dir.join(CRUSH_DB_FILE), project_path));
        }
    }
    Ok(candidates)
}

fn crush_global_state_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(CRUSH_DATA_DIR),
        context.home.join(".local/share/crush/crush.json"),
        crush_projects_index(context),
    ]
}

fn crush_projects_index(context: &HarnessOpsContext) -> PathBuf {
    context.home.join(".local/share/crush").join(PROJECTS_INDEX)
}

fn path_scope_matches(candidate_cwd: &Path, old_path: &Path) -> bool {
    candidate_cwd == old_path
        || old_path.starts_with(candidate_cwd)
        || candidate_cwd.starts_with(old_path)
}

fn table_names(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut tables = BTreeSet::new();
    for row in rows {
        tables.insert(row?);
    }
    Ok(tables)
}

fn table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = BTreeSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn query_count(conn: &Connection, sql: &str) -> Result<usize> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))? as usize)
}

fn count_sqlite_text_refs(conn: &Connection, table: &str, needles: &[String]) -> Result<usize> {
    if needles.is_empty() {
        return Ok(0);
    }
    let columns = table_columns(conn, table)?;
    if columns.is_empty() {
        return Ok(0);
    }

    let select_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {select_list} FROM {}", quote_ident(table));
    let mut stmt = conn.prepare(&sql)?;
    let column_count = columns.len();
    let mut rows = stmt.query([])?;
    let mut refs = 0;

    while let Some(row) = rows.next()? {
        for idx in 0..column_count {
            match row.get_ref(idx)? {
                ValueRef::Text(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    if needles.iter().any(|needle| text.contains(needle)) {
                        refs += 1;
                    }
                }
                ValueRef::Blob(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    if needles.iter().any(|needle| text.contains(needle)) {
                        refs += 1;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(refs)
}

fn count_text_file_refs(path: &Path, needles: &[String]) -> Result<usize> {
    if !path.exists() || needles.is_empty() {
        return Ok(0);
    }
    let content = fs::read_to_string(path)?;
    Ok(usize::from(
        needles.iter().any(|needle| content.contains(needle)),
    ))
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn crush_sqlite_facts_distinguish_session_identity_from_path_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("repo");
        let new = home.join("repo-new");
        fs::create_dir_all(old.join(".crush")).unwrap();
        let db_path = old.join(".crush/crush.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                parent_session_id TEXT,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                cost REAL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE messages (
                session_id TEXT,
                role TEXT,
                parts TEXT,
                created_at INTEGER,
                model TEXT,
                provider TEXT
            )",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE files (session_id TEXT, path TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, parent_session_id) VALUES (?1, ?2, ?3)",
            params!["sess-1", "Work on repo", Option::<String>::None],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, parent_session_id) VALUES (?1, ?2, ?3)",
            params!["tool-call-1", "Helper", "sess-1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, parts, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                "sess-1",
                "user",
                format!(r#"[{{"type":"text","text":"edit {}"}}]"#, old.display()),
                1_i64
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (session_id, path) VALUES (?1, ?2)",
            params!["sess-1", old.join("src/main.rs").display().to_string()],
        )
        .unwrap();
        drop(conn);

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();

        assert_eq!(report.harness, AgentKind::Crush);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 2);
        assert_eq!(report.path_references_found, 2);
        assert!(report.edits.iter().all(|edit| !edit.apply_ready));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("SQLite crush.db")));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("child/helper/task-like")));
    }

    #[test]
    fn project_index_candidates_are_doctor_roots_and_text_ref_intents() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("workspace/old");
        let new = home.join("workspace/new");
        let data_dir = home.join("external-crush");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(home.join(".local/share/crush")).unwrap();
        fs::write(
            crush_projects_index(&HarnessOpsContext::from_home(home.to_path_buf())),
            format!(
                r#"{{"projects":[{{"path":"{}","data_dir":"{}"}}]}}"#,
                old.display(),
                data_dir.display()
            ),
        )
        .unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();

        assert_eq!(report.path_references_found, 1);
        assert!(report.operations.iter().any(|operation| {
            operation.harness == AgentKind::Crush
                && operation.action == "rewrite_project_index_refs"
                && !operation.apply_ready
        }));
    }
}
