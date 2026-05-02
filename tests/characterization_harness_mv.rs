use std::fs;
use std::path::Path;

use babel::agent_kind::AgentKind;
use babel::harness_ops::{
    apply_migration_plan, plan_migration_with_context, AdapterReadiness, ApplyCapability,
    HarnessOpsContext, MigrationApplyOptions, MigrationEditKind, RecoveryClass,
};
use rusqlite::params;

fn write_file(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn codex_base(home: &Path) -> std::path::PathBuf {
    home.join(".codex")
}

fn codex_sessions(home: &Path) -> std::path::PathBuf {
    home.join(".codex/sessions")
}

fn codex_shell_snapshots(home: &Path) -> std::path::PathBuf {
    home.join(".codex/shell_snapshots")
}

#[test]
fn codex_doctor_report_surfaces_typed_move_edits_from_native_state() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("work/project");
    let nested = old.join("crates/core");
    let new = home.join("work/project-renamed");
    fs::create_dir_all(&nested).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let rollout =
        codex_sessions(home).join("2026/05/01/rollout-2026-05-01T12-00-00-session-a.jsonl");
    write_file(
        &rollout,
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"session-a\",\"cwd\":\"{}\"}}}}\n\
             {{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\",\"collaboration_mode\":{{\"mode\":\"plan\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"message\":\"saw {}\"}}}}\n",
            old.display(),
            nested.display(),
            old.display(),
        ),
    );
    write_file(
        &codex_base(home).join("history.jsonl"),
        &format!(
            "{{\"session_id\":\"session-a\",\"text\":\"{}\"}}\n",
            old.display()
        ),
    );
    write_file(
        &codex_base(home).join("session_index.jsonl"),
        &format!(
            "{{\"id\":\"session-a\",\"note\":\"current code counts this {} reference\"}}\n",
            old.display()
        ),
    );
    write_file(
        &codex_base(home).join("config.toml"),
        &format!(
            "sqlite_home = \"dbs\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            old.display()
        ),
    );
    write_file(
        &codex_base(home).join("config.json"),
        &format!("{{\"last_project\":\"{}\"}}\n", old.display()),
    );
    write_file(
        &codex_base(home).join("internal_storage.json"),
        &format!("{{\"cwd\":\"{}\"}}\n", old.display()),
    );
    write_file(
        &codex_shell_snapshots(home).join("session-a.1.sh"),
        &format!("cd {}\n", old.display()),
    );
    let state_db = codex_base(home).join("dbs/state_5.sqlite");
    fs::create_dir_all(state_db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&state_db).unwrap();
    conn.execute(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
        params![
            "session-a",
            rollout.to_string_lossy(),
            old.to_string_lossy()
        ],
    )
    .unwrap();
    drop(conn);

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    let codex = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Codex)
        .unwrap();

    assert_eq!(report.old_path, old);
    assert_eq!(report.new_path, new);
    assert!(matches!(codex.readiness, AdapterReadiness::ApplyReady));
    assert_eq!(codex.sessions_found, 1);
    assert_eq!(codex.path_references_found, 7);
    assert!(codex
        .notes
        .iter()
        .any(|note| note.contains("session_meta.payload.cwd")));
    assert!(codex
        .notes
        .iter()
        .any(|note| note.contains("matched Codex session id(s): session-a")));

    let actions = codex
        .edits
        .iter()
        .map(|edit| edit.action.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        vec![
            "rewrite_session_meta_cwd",
            "rewrite_turn_context_cwd",
            "rewrite_project_config_keys",
            "rewrite_project_config_refs",
            "rewrite_internal_storage_refs",
            "rewrite_thread_index_cwd",
            "rewrite_history_path_refs",
            "rewrite_session_index_path_refs",
            "preserve_session_shell_snapshots",
        ]
    );

    let session_meta = codex
        .edits
        .iter()
        .find(|edit| edit.action == "rewrite_session_meta_cwd")
        .unwrap();
    assert_eq!(session_meta.capability, ApplyCapability::ApplyReady);
    assert_eq!(session_meta.recovery, RecoveryClass::OwnedFile);
    assert!(matches!(
        &session_meta.kind,
        MigrationEditKind::RewriteJsonlField { files, count, .. }
            if files == &vec![rollout.clone()] && *count == 1
    ));

    let sqlite = codex
        .edits
        .iter()
        .find(|edit| edit.action == "rewrite_thread_index_cwd")
        .unwrap();
    assert_eq!(sqlite.capability, ApplyCapability::ApplyReady);
    assert_eq!(sqlite.recovery, RecoveryClass::SessionDependencyFile);
    assert!(matches!(
        &sqlite.kind,
        MigrationEditKind::RewriteSqliteTextColumn { path, table, column, count, .. }
            if path == &state_db && table == "threads" && column == "cwd" && *count == 1
    ));

    let snapshots = codex
        .edits
        .iter()
        .find(|edit| edit.action == "preserve_session_shell_snapshots")
        .unwrap();
    assert_eq!(snapshots.capability, ApplyCapability::PreserveOnly);
    assert!(matches!(
        &snapshots.kind,
        MigrationEditKind::PreserveSessionKeyedFiles {
            session_count,
            path_ref_count,
            ..
        } if *session_count == 1 && *path_ref_count == 1
    ));
}

#[test]
fn codex_apply_dry_run_reports_executor_edits_without_mutating_state() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("project");
    let new = home.join("project-renamed");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let rollout =
        codex_sessions(home).join("2026/05/01/rollout-2026-05-01T12-00-00-session-a.jsonl");
    let rollout_before = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"session-a\",\"cwd\":\"{}\"}}}}\n",
        old.display()
    );
    write_file(&rollout, &rollout_before);
    let config = codex_base(home).join("config.toml");
    let config_before = format!(
        "sqlite_home = \"{}\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        codex_base(home).join("sqlite-home").display(),
        old.display()
    );
    write_file(&config, &config_before);
    let state_db = codex_base(home).join("sqlite-home/state_5.sqlite");
    fs::create_dir_all(state_db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&state_db).unwrap();
    conn.execute(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
        params![
            "session-a",
            rollout.to_string_lossy(),
            old.to_string_lossy()
        ],
    )
    .unwrap();
    drop(conn);

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: true,
            force: false,
            transaction_root: Some(home.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(apply.dry_run);
    assert_eq!(apply.edits_seen, 4);
    assert_eq!(apply.edits_apply_ready, 3);
    assert_eq!(apply.applied, vec!["would apply 3 executor-owned edit(s)"]);
    assert_eq!(
        apply.skipped,
        vec!["aider:preserve_project_local_history is preserve-only"]
    );
    assert!(apply.manifest_path.is_none());
    assert!(apply.verified.is_empty());
    assert!(home.join("transactions").read_dir().is_err());
    assert_eq!(fs::read_to_string(&rollout).unwrap(), rollout_before);
    assert_eq!(fs::read_to_string(&config).unwrap(), config_before);

    let conn = rusqlite::Connection::open(&state_db).unwrap();
    let cwd: String = conn
        .query_row(
            "SELECT cwd FROM threads WHERE id = 'session-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cwd, old.display().to_string());
}
