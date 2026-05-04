use super::claude::{claude_encode_cc_port, claude_encode_ccmv, count_history_refs};
use super::planner::{
    absolute_path, migration_planners_for_scope, plan_migration_with_context_and_scope,
    MigrationPlanScope, MIGRATION_PLANNERS,
};
use super::*;
use crate::agent_kind::{AgentKind, HarnessSupport};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::ops::ControlFlow;

fn write_file(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    file.write_all(content.as_bytes()).unwrap();
}

#[test]
fn migration_planner_registry_covers_each_real_harness_once() {
    let planned = migration_planners_for_scope(MigrationPlanScope::Doctor)
        .map(|planner| planner.kind)
        .collect::<Vec<_>>();
    let registered: BTreeSet<_> = planned.iter().map(|kind| kind.slug()).collect();
    let canonical: BTreeSet<_> = AgentKind::ALL.iter().map(|kind| kind.slug()).collect();

    assert_eq!(planned.len(), AgentKind::ALL.len());
    assert_eq!(registered.len(), AgentKind::ALL.len());
    assert_eq!(
        registered, canonical,
        "doctor planning roster should cover every AgentKind exactly once"
    );
    assert_eq!(
        &planned[..3],
        &[AgentKind::Claude, AgentKind::Codex, AgentKind::Aider],
        "apply-ready planners run first so apply scope is a strict prefix of doctor scope"
    );
}

#[test]
fn apply_scope_registry_only_runs_mutation_ready_planners() {
    let planned = migration_planners_for_scope(MigrationPlanScope::Apply)
        .map(|planner| planner.kind)
        .collect::<Vec<_>>();

    assert_eq!(
        planned,
        vec![AgentKind::Claude, AgentKind::Codex, AgentKind::Aider]
    );
}

#[test]
fn claude_doctor_reports_project_and_history_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("Workspace/old");
    let new = home.join("Workspace/new");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let old_project = ctx
        .claude_base()
        .join("projects")
        .join(claude_encode_cc_port(&old));
    write_file(&old_project.join("session-a.jsonl"), "{}\n");
    write_file(
        &ctx.claude_base().join("history.jsonl"),
        &format!(
            "{{\"project\":\"{}\",\"sessionId\":\"session-a\",\"timestamp\":1,\"display\":\"x\"}}\n",
            old.display()
        ),
    );

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    let claude = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Claude)
        .unwrap();

    assert_eq!(claude.sessions_found, 1);
    assert_eq!(claude.path_references_found, 1);
    assert!(claude
        .operations
        .iter()
        .any(|op| op.action == "rename_project_dir" && op.apply_ready));
    assert!(claude.edits.iter().any(|edit| {
        edit.action == "rename_project_dir"
            && matches!(&edit.kind, MigrationEditKind::RenamePath { .. })
    }));
    assert!(claude
        .operations
        .iter()
        .any(|op| op.action == "rewrite_history_paths" && op.apply_ready));
    assert!(claude.edits.iter().any(|edit| {
        edit.action == "rewrite_history_paths"
            && matches!(&edit.kind, MigrationEditKind::RewriteJsonlField { .. })
    }));
    assert!(!report.has_blockers());
}

#[test]
fn doctor_blocks_nested_destinations_and_existing_claude_target() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("repo");
    let new = old.join("nested");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let old_project = ctx
        .claude_base()
        .join("projects")
        .join(claude_encode_cc_port(&old));
    fs::create_dir_all(old_project).unwrap();
    let new_project = ctx
        .claude_base()
        .join("projects")
        .join(claude_encode_cc_port(&new));
    fs::create_dir_all(new_project).unwrap();

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    assert!(report.has_blockers());
    assert!(report
        .risks
        .iter()
        .any(|risk| risk.message.contains("nested inside source")));
    assert!(report
        .risks
        .iter()
        .any(|risk| risk.message.contains("destination project folder")));
}

#[test]
fn doctor_treats_existing_claude_destination_as_already_applied_when_source_key_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("repositories/oomfi");
    let new = home.join("repo-py/oomfi");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let new_project = ctx
        .claude_base()
        .join("projects")
        .join(claude_encode_cc_port(&new));
    write_file(&new_project.join("session-a.jsonl"), "{}\n");

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    let claude = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Claude)
        .unwrap();

    assert!(!report.has_blockers());
    assert!(!claude
        .edits
        .iter()
        .any(|edit| edit.action == "rename_project_dir"));
    assert!(claude
        .notes
        .iter()
        .any(|note| note.contains("already applied")));
}

#[test]
fn claude_project_keys_ignore_trailing_slashes() {
    let path = Path::new("/home/example/projects/babel/");
    assert_eq!(claude_encode_cc_port(path), "-home-example-projects-babel");
    assert_eq!(claude_encode_ccmv(path), "-home-example-projects-babel");
}

#[test]
fn absolute_path_normalizes_dot_dot_without_existing_destination() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        absolute_path(Path::new("repo/../repo-tool/pomet")),
        cwd.join("repo-tool/pomet")
    );
}

#[test]
fn codex_uses_native_session_identity_and_project_config() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("project");
    let new = home.join("project-renamed");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    write_file(
        &ctx.codex_sessions()
            .join("2026/04/29/rollout-2026-04-29T12-00-00-codex-session.jsonl"),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\",\"collaboration_mode\":{{\"mode\":\"plan\"}}}}}}\n",
            old.display(),
            old.display()
        ),
    );
    write_file(
        &ctx.codex_sessions()
            .join("2026/04/29/rollout-2026-04-29T12-00-00-unrelated.jsonl"),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"unrelated\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"{}\"}}}}\n",
            home.join("other").display(),
            old.display()
        ),
    );
    write_file(
        &ctx.codex_base().join("history.jsonl"),
        &format!(
            "{{\"session_id\":\"codex-session\",\"text\":\"{}\"}}\n",
            old.display()
        ),
    );
    write_file(
        &ctx.codex_base().join("config.toml"),
        &format!(
            "sqlite_home = \"{}\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            ctx.codex_base().join("sqlite-home").display(),
            old.display(),
        ),
    );
    {
        let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
        fs::create_dir_all(state_db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&state_db).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, cwd) VALUES (?1, ?2)",
            rusqlite::params!["codex-session", old.to_string_lossy()],
        )
        .unwrap();
    }
    write_file(
        &ctx.codex_shell_snapshots().join("codex-session.1.sh"),
        &format!("cd {}\n", old.display()),
    );
    write_file(
        &ctx.gemini_tmp().join("hash/chats/session.json"),
        &format!("{{\"project\":\"{}\"}}\n", old.display()),
    );

    let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
    let codex = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Codex)
        .unwrap();
    assert_eq!(codex.sessions_found, 1);
    assert_eq!(codex.path_references_found, 6);
    assert!(matches!(codex.readiness, AdapterReadiness::ApplyReady));
    assert!(codex.operations.iter().any(|op| op.apply_ready));
    assert!(codex
        .operations
        .iter()
        .any(|op| op.action == "rewrite_session_meta_cwd"));
    assert!(codex.edits.iter().any(|edit| {
        edit.action == "rewrite_session_meta_cwd"
            && matches!(&edit.kind, MigrationEditKind::RewriteJsonlField { .. })
    }));
    assert!(codex.edits.iter().any(|edit| {
        edit.action == "rewrite_session_path_refs"
            && matches!(
                &edit.kind,
                MigrationEditKind::RewriteTextRefs { files, .. }
                if files.len() == 2
            )
    }));
    assert!(codex
        .operations
        .iter()
        .any(|op| op.action == "rewrite_project_config_keys"));
    assert!(codex.edits.iter().any(|edit| {
        edit.action == "rewrite_project_config_keys"
            && matches!(&edit.kind, MigrationEditKind::RewriteTomlTableKey { .. })
    }));
    assert!(codex
        .operations
        .iter()
        .any(|op| op.action == "rewrite_thread_index_cwd"
            && op.target.ends_with(".codex/sqlite-home/state_5.sqlite")
            && op.apply_ready));
    assert!(codex.edits.iter().any(|edit| {
        edit.action == "rewrite_thread_index_cwd"
            && matches!(
                &edit.kind,
                MigrationEditKind::RewriteSqliteTextColumn { .. }
            )
    }));
    assert!(codex
        .operations
        .iter()
        .any(|op| op.action == "preserve_session_shell_snapshots"));
    assert!(codex.edits.iter().any(|edit| {
        edit.action == "preserve_session_shell_snapshots"
            && matches!(
                &edit.kind,
                MigrationEditKind::PreserveSessionKeyedFiles { .. }
            )
    }));

    let gemini = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Gemini)
        .unwrap();
    assert_eq!(gemini.path_references_found, 1);
}

#[test]
fn codex_project_config_key_requires_actual_toml_header() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("project");
    let new = home.join("project-renamed");
    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    write_file(
        &ctx.codex_base().join("config.toml"),
        &format!("# stale comment mentioning {}\n", old.display()),
    );

    let report = plan_migration_with_context_and_scope(
        &ctx,
        &old,
        &new,
        Vec::new(),
        MigrationPlanScope::Apply,
    )
    .unwrap();
    let codex = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Codex)
        .unwrap();

    assert!(!codex
        .edits
        .iter()
        .any(|edit| edit.action == "rewrite_project_config_keys"));
}

#[test]
fn codex_indexed_planning_uses_thread_index_without_rollout_tree_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("project");
    let new = home.join("project-renamed");
    fs::create_dir_all(&old).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let matched_rollout = ctx
        .codex_sessions()
        .join("2026/04/29/rollout-2026-04-29T12-00-00-codex-session.jsonl");
    write_file(
        &matched_rollout,
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n",
            old.display(),
        ),
    );
    write_file(
        &ctx.codex_sessions()
            .join("2026/04/29/rollout-2026-04-29T12-00-00-unrelated.jsonl"),
        &format!(
            "{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"unrelated body mention {}\"}}}}\n",
            old.display(),
        ),
    );
    write_file(
        &ctx.codex_base().join("config.toml"),
        &format!(
            "sqlite_home = \"{}\"\n",
            ctx.codex_base().join("sqlite-home").display(),
        ),
    );
    {
        let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
        fs::create_dir_all(state_db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&state_db).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "codex-session",
                matched_rollout.to_string_lossy(),
                old.to_string_lossy(),
            ],
        )
        .unwrap();
    }

    for scope in [MigrationPlanScope::Apply, MigrationPlanScope::Doctor] {
        let report =
            plan_migration_with_context_and_scope(&ctx, &old, &new, Vec::new(), scope).unwrap();
        let codex = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Codex)
            .unwrap();

        assert_eq!(codex.sessions_found, 1);
        assert!(!codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_session_path_refs"));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_session_meta_cwd"
                && matches!(
                    &edit.kind,
                    MigrationEditKind::RewriteJsonlField { files, .. }
                    if files == &vec![matched_rollout.clone()]
                )
        }));
    }
}

#[test]
fn codex_apply_repairs_alias_cwd_surfaces_without_original_source() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("repo-tool/pomet");
    let old_alias = home.join("repo/../repo-tool/pomet");
    let new = home.join("repo/pomet");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();

    let ctx = HarnessOpsContext::from_home(home.to_path_buf());
    let matched_rollout = ctx
        .codex_sessions()
        .join("2026/05/01/rollout-2026-05-01T13-09-14-codex-session.jsonl");
    write_file(
        &matched_rollout,
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
            old_alias.display(),
            new.display(),
        ),
    );
    write_file(
        &ctx.codex_base().join("config.toml"),
        &format!(
            "sqlite_home = \"{}\"\n",
            ctx.codex_base().join("sqlite-home").display(),
        ),
    );
    let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
    {
        fs::create_dir_all(state_db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&state_db).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "codex-session",
                matched_rollout.to_string_lossy(),
                old.to_string_lossy(),
            ],
        )
        .unwrap();
    }

    let report = plan_migration_with_context_and_scope(
        &ctx,
        &old,
        &new,
        Vec::new(),
        MigrationPlanScope::Apply,
    )
    .unwrap();
    let codex = report
        .harnesses
        .iter()
        .find(|harness| harness.harness == AgentKind::Codex)
        .unwrap();

    assert!(codex.edits.iter().any(|edit| {
        edit.action == "rewrite_session_meta_cwd"
            && matches!(
                &edit.kind,
                MigrationEditKind::RewriteJsonlField { from, files, .. }
                if from == &old_alias.display().to_string()
                    && files == &vec![matched_rollout.clone()]
            )
    }));
    assert!(!codex
        .edits
        .iter()
        .any(|edit| edit.action == "rewrite_turn_context_cwd"));

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(home.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(!apply.has_blockers());
    let rollout = fs::read_to_string(&matched_rollout).unwrap();
    assert!(rollout.contains(&format!("\"cwd\":\"{}\"", new.display())));
    assert!(!rollout.contains(&old_alias.display().to_string()));

    let conn = rusqlite::Connection::open(&state_db).unwrap();
    let cwd: String = conn
        .query_row(
            "SELECT cwd FROM threads WHERE id = 'codex-session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cwd, new.display().to_string());
}

#[test]
fn generic_apply_rewrites_only_prescanned_text_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(root.join("state/day")).unwrap();

    let matching = root.join("state/day/matching.jsonl");
    let unrelated = root.join("state/day/unrelated.jsonl");
    write_file(&matching, &format!("cwd={}\n", old.display()));
    write_file(&unrelated, &format!("cwd={}\n", old.display()));

    let edit = MigrationEdit::rewrite_text_refs_in_files(
        AgentKind::Codex,
        "rewrite_session_path_refs",
        root.join("state").display().to_string(),
        vec![matching.clone()],
        old.display().to_string(),
        new.display().to_string(),
        1,
    )
    .with_apply_ready(true);
    let report = MigrationDoctorReport {
        old_path: old.clone(),
        new_path: new.clone(),
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Codex,
            AdapterReadiness::ApplyReady,
            vec![root.join("state")],
            0,
            1,
            vec![edit],
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(!apply.has_blockers());
    assert_eq!(apply.verified.len(), 1);
    assert!(fs::read_to_string(&matching)
        .unwrap()
        .contains(&new.display().to_string()));
    assert!(fs::read_to_string(&unrelated)
        .unwrap()
        .contains(&old.display().to_string()));
}

#[test]
fn generic_apply_consumes_typed_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(&old).unwrap();

    let jsonl = root.join("history.jsonl");
    write_file(
        &jsonl,
        &format!(
            "{{\"project\":\"{}\",\"display\":\"x\",\"collaboration_mode\":{{\"mode\":\"plan\"}}}}\n",
            old.display()
        ),
    );
    let toml = root.join("config.toml");
    write_file(
        &toml,
        &format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            old.display()
        ),
    );
    let text = root.join("notes.txt");
    write_file(&text, &format!("cwd={}\n", old.display()));
    let sqlite = root.join("state_5.sqlite");
    {
        let conn = rusqlite::Connection::open(&sqlite).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, cwd) VALUES (?1, ?2)",
            rusqlite::params!["session", old.to_string_lossy()],
        )
        .unwrap();
    }

    let edits = vec![
        MigrationEdit::rewrite_jsonl_field(
            AgentKind::Claude,
            "rewrite_history_paths",
            jsonl.clone(),
            "$.project",
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
        MigrationEdit::rewrite_toml_table_key(
            AgentKind::Codex,
            "rewrite_project_config_keys",
            toml.clone(),
            "projects",
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
        MigrationEdit::rewrite_text_refs(
            AgentKind::Gemini,
            "rewrite_native_path_refs",
            text.display().to_string(),
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
        MigrationEdit::rewrite_sqlite_text_column(
            AgentKind::Codex,
            "rewrite_thread_index_cwd",
            sqlite.clone(),
            "threads",
            "cwd",
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
    ];
    let report = MigrationDoctorReport {
        old_path: old.clone(),
        new_path: new.clone(),
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Claude,
            AdapterReadiness::ApplyReady,
            vec![root.to_path_buf()],
            0,
            4,
            edits,
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();
    assert_eq!(apply.edits_seen, 4);
    assert!(!apply.has_blockers());
    assert!(fs::read_to_string(&jsonl)
        .unwrap()
        .contains(&new.display().to_string()));
    assert!(fs::read_to_string(&jsonl)
        .unwrap()
        .contains("\"mode\":\"plan\""));
    assert!(fs::read_to_string(&toml)
        .unwrap()
        .contains(&new.display().to_string()));
    assert!(fs::read_to_string(&text)
        .unwrap()
        .contains(&new.display().to_string()));
    let conn = rusqlite::Connection::open(sqlite).unwrap();
    let cwd: String = conn
        .query_row("SELECT cwd FROM threads WHERE id = 'session'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cwd, new.to_string_lossy().to_string());
    assert!(apply.manifest_path.unwrap().exists());
    assert_eq!(apply.verified.len(), 4);
}

#[test]
fn generic_apply_preserves_rewritten_file_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old");
    let new = root.join("new");
    let text = root.join("history.jsonl");
    write_file(&text, &format!("cwd={}\n", old.display()));
    let original_time = filetime::FileTime::from_unix_time(1_700_000_000, 123_000_000);
    filetime::set_file_times(&text, original_time, original_time).unwrap();

    let edit = MigrationEdit::rewrite_text_refs(
        AgentKind::Codex,
        "rewrite_history_path_refs",
        text.display().to_string(),
        old.display().to_string(),
        new.display().to_string(),
        1,
    )
    .with_apply_ready(true);
    let report = MigrationDoctorReport {
        old_path: old,
        new_path: new.clone(),
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Codex,
            AdapterReadiness::ApplyReady,
            vec![root.to_path_buf()],
            0,
            1,
            vec![edit],
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(!apply.has_blockers());
    assert!(fs::read_to_string(&text)
        .unwrap()
        .contains(&new.display().to_string()));
    let metadata = fs::metadata(&text).unwrap();
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&metadata),
        original_time
    );
}

#[test]
fn generic_apply_verifies_only_mutated_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old");
    let new = root.join("new");
    let text = root.join("history.jsonl");
    let toml = root.join("config.toml");
    write_file(&text, &format!("cwd={}\n", old.display()));
    write_file(&toml, "# no project table here\n");

    let edits = vec![
        MigrationEdit::rewrite_text_refs(
            AgentKind::Codex,
            "rewrite_history_path_refs",
            text.display().to_string(),
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
        MigrationEdit::rewrite_toml_table_key(
            AgentKind::Codex,
            "rewrite_project_config_keys",
            toml.clone(),
            "projects",
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true),
    ];
    let report = MigrationDoctorReport {
        old_path: old,
        new_path: new.clone(),
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Codex,
            AdapterReadiness::ApplyReady,
            vec![root.to_path_buf()],
            0,
            2,
            edits,
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(!apply.has_blockers());
    assert_eq!(apply.verified, vec!["codex:rewrite_history_path_refs"]);
    assert!(apply
        .skipped
        .iter()
        .any(|line| line.contains("no TOML table key matched")));
    assert!(fs::read_to_string(&text)
        .unwrap()
        .contains(&new.display().to_string()));
}

#[test]
fn generic_apply_skips_preserve_only_edits_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let report = MigrationDoctorReport {
        old_path: root.join("old"),
        new_path: root.join("new"),
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Aider,
            AdapterReadiness::DoctorOnly,
            vec![root.to_path_buf()],
            1,
            0,
            vec![MigrationEdit::preserve_project_local_history(
                AgentKind::Aider,
                root.display().to_string(),
                "project-local files follow the move",
            )],
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert_eq!(apply.edits_seen, 1);
    assert_eq!(apply.edits_apply_ready, 0);
    assert!(!apply.has_blockers());
    assert!(apply.manifest_path.is_none());
    assert_eq!(apply.skipped.len(), 1);
}

#[test]
fn generic_apply_treats_completed_rename_as_idempotent_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old-project-key");
    let new = root.join("new-project-key");
    fs::create_dir_all(&new).unwrap();

    let edit = MigrationEdit::rename_path(
        AgentKind::Claude,
        "rename_project_dir",
        old.clone(),
        new.clone(),
        "preserve Claude transcript files",
    )
    .with_apply_ready(true)
    .with_recovery(RecoveryClass::SessionDependencyDir);
    let report = MigrationDoctorReport {
        old_path: old,
        new_path: new,
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Claude,
            AdapterReadiness::ApplyReady,
            vec![root.to_path_buf()],
            0,
            0,
            vec![edit],
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let apply = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap();

    assert!(!apply.has_blockers());
    assert!(apply.verified.is_empty());
    assert!(apply
        .skipped
        .iter()
        .any(|line| line.contains("already applied")));
}

#[test]
fn generic_apply_rolls_back_owned_files_when_verification_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(&old).unwrap();
    let text = root.join("notes.txt");
    write_file(&text, &format!("cwd={}\n", old.display()));

    let mut edit = MigrationEdit::rewrite_text_refs(
        AgentKind::Gemini,
        "rewrite_native_path_refs",
        text.display().to_string(),
        old.display().to_string(),
        new.display().to_string(),
        1,
    )
    .with_apply_ready(true);
    edit.verification = VerificationSpec::TextRefsReduced {
        target: text.display().to_string(),
        files: Vec::new(),
        from: new.display().to_string(),
        to: old.display().to_string(),
        expected_removed_min: 1,
    };

    let report = MigrationDoctorReport {
        old_path: old.clone(),
        new_path: new,
        indexing_policy: "test".to_string(),
        live_panes: Vec::new(),
        harnesses: vec![HarnessMigrationReport::from_edits(
            AgentKind::Gemini,
            AdapterReadiness::ApplyReady,
            vec![root.to_path_buf()],
            0,
            1,
            vec![edit],
            Vec::new(),
        )],
        risks: Vec::new(),
    };

    let error = apply_migration_plan(
        &report,
        &MigrationApplyOptions {
            dry_run: false,
            force: false,
            transaction_root: Some(root.join("transactions")),
            print_progress: false,
            progress_bars: false,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("rolled back"));
    assert_eq!(
        fs::read_to_string(&text).unwrap(),
        format!("cwd={}\n", old.display())
    );
}

fn repo_tempdir() -> tempfile::TempDir {
    fs::create_dir_all("tmp").ok();
    tempfile::Builder::new()
        .prefix("harness_ops_migration_")
        .tempdir_in("tmp")
        .expect("create tempdir under repo-local tmp/")
}

#[test]
fn doctor_scope_includes_every_agent_kind_exactly_once_end_to_end() {
    let tmp = repo_tempdir();
    let home = tmp.path();
    let old = home.join("Workspace/old");
    let new = home.join("Workspace/new");
    fs::create_dir_all(&old).unwrap();
    let ctx = HarnessOpsContext::from_home(home.to_path_buf());

    let report = plan_migration_with_context_and_scope(
        &ctx,
        &old,
        &new,
        Vec::new(),
        MigrationPlanScope::Doctor,
    )
    .unwrap();

    let kinds: Vec<AgentKind> = report
        .harnesses
        .iter()
        .map(|harness| harness.harness)
        .collect();
    let unique: BTreeSet<&str> = kinds.iter().map(|kind| kind.slug()).collect();

    assert_eq!(
        kinds.len(),
        AgentKind::ALL.len(),
        "doctor report must list every harness exactly once"
    );
    assert_eq!(
        unique.len(),
        AgentKind::ALL.len(),
        "doctor report contains duplicate AgentKind entries"
    );
    for kind in AgentKind::ALL {
        assert!(
            kinds.contains(kind),
            "doctor report missing coverage for {}",
            kind.slug()
        );
    }
}

#[test]
fn apply_scope_includes_only_apply_planned_adapters_end_to_end() {
    let tmp = repo_tempdir();
    let home = tmp.path();
    let old = home.join("Workspace/old");
    let new = home.join("Workspace/new");
    fs::create_dir_all(&old).unwrap();
    let ctx = HarnessOpsContext::from_home(home.to_path_buf());

    let report = plan_migration_with_context_and_scope(
        &ctx,
        &old,
        &new,
        Vec::new(),
        MigrationPlanScope::Apply,
    )
    .unwrap();

    let kinds: Vec<AgentKind> = report
        .harnesses
        .iter()
        .map(|harness| harness.harness)
        .collect();

    assert_eq!(
        kinds,
        vec![AgentKind::Claude, AgentKind::Codex, AgentKind::Aider],
        "apply scope must mutate only the Claude/Codex/Aider adapter set; \
         pin or update this assertion if the apply-capable roster intentionally changes"
    );
    assert_eq!(
        report.harnesses.len(),
        3,
        "apply scope should not pad the report with doctor-fill entries"
    );
}

#[test]
fn supported_operation_harnesses_pins_supported_subset() {
    let supported = supported_operation_harnesses();

    let expected: Vec<AgentKind> = MIGRATION_PLANNERS
        .iter()
        .map(|planner| planner.kind)
        .filter(|kind| !matches!(kind.spec().support, HarnessSupport::Unsupported))
        .collect();
    assert_eq!(
        supported, expected,
        "supported_operation_harnesses() must mirror MIGRATION_PLANNERS minus Unsupported in registry order"
    );

    assert!(
        supported
            .iter()
            .all(|kind| !matches!(kind.spec().support, HarnessSupport::Unsupported)),
        "supported_operation_harnesses() must drop AgentKinds whose spec marks them Unsupported"
    );

    let supported_set: BTreeSet<&str> = supported.iter().map(|kind| kind.slug()).collect();
    assert_eq!(
        supported_set.len(),
        supported.len(),
        "no duplicate kinds in supported roster"
    );

    for unsupported_in_registry in MIGRATION_PLANNERS
        .iter()
        .filter(|planner| matches!(planner.kind.spec().support, HarnessSupport::Unsupported))
        .map(|planner| planner.kind)
    {
        assert!(
            !supported.contains(&unsupported_in_registry),
            "supported_operation_harnesses() must not surface {} (Unsupported)",
            unsupported_in_registry.slug()
        );
    }
}

#[test]
fn for_each_jsonl_value_skips_blanks_and_garbage_and_respects_cap_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scan.jsonl");
    write_file(
        &path,
        "{\"i\":1}\n\
         \n\
         not json at all\n\
         {\"i\":2}\n\
         {\"i\":3}\n\
         {\"i\":4}\n",
    );

    let mut seen = Vec::new();
    let outcome = for_each_jsonl_value::<()>(&path, Some(4), |value| {
        seen.push(value.get("i").and_then(|v| v.as_i64()).unwrap_or(-1));
        ControlFlow::Continue(())
    })
    .unwrap();

    assert!(outcome.is_none());
    // Cap of 4 covers the first four physical lines (one of which is blank
    // and another is malformed); only the parsed values reach the visitor,
    // and order is preserved.
    assert_eq!(seen, vec![1, 2]);
}

#[test]
fn for_each_jsonl_value_break_short_circuits_before_later_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("break.jsonl");
    write_file(
        &path,
        "{\"i\":1}\n\
         {\"i\":2,\"stop\":true}\n\
         {\"i\":3}\n\
         {\"i\":4}\n",
    );

    let mut visits = 0usize;
    let outcome = for_each_jsonl_value::<i64>(&path, None, |value| {
        visits += 1;
        if value.get("stop").and_then(|v| v.as_bool()).unwrap_or(false) {
            let i = value.get("i").and_then(|v| v.as_i64()).unwrap();
            ControlFlow::Break(i)
        } else {
            ControlFlow::Continue(())
        }
    })
    .unwrap();

    assert_eq!(outcome, Some(2));
    // Visitor must not run on records after the decisive Break, and the
    // helper must stop reading lines past that point.
    assert_eq!(visits, 2);
}

#[test]
fn count_history_refs_matches_exact_and_child_prefix_only() {
    let tmp = tempfile::tempdir().unwrap();
    let history = tmp.path().join("history.jsonl");
    let old = Path::new("/home/u/Workspace/old");

    write_file(
        &history,
        "{\"project\":\"/home/u/Workspace/old\"}\n\
         {\"project\":\"/home/u/Workspace/old/sub\"}\n\
         {\"project\":\"/home/u/Workspace/old-sibling\"}\n\
         {\"project\":\"/home/u/Workspace/older\"}\n\
         {\"no_project\":\"x\"}\n\
         not even json\n\
         \n\
         {\"project\":42}\n",
    );

    let count = count_history_refs(&history, old).unwrap();
    // Counts the exact match plus the child-prefix match. Sibling and
    // longer-name paths must not match. Malformed / missing / wrong-typed
    // project rows are silently ignored.
    assert_eq!(count, 2);
}
