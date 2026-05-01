use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

use crate::agent_kind::AgentKind;

use super::{
    is_probably_text_state_file, text_file_contains_any, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, MigrationEdit, MAX_SCAN_BYTES, MAX_SCAN_FILES,
};

#[derive(Default)]
struct CodexDiscovery {
    session_roots: Vec<PathBuf>,
    matched_sessions: Vec<CodexSession>,
    session_path_ref_files: usize,
    matched_session_path_ref_files: Vec<PathBuf>,
    history_ref_entries: usize,
    session_index_ref_entries: usize,
    config_toml_ref_files: usize,
    config_json_ref_files: usize,
    internal_storage_ref_files: usize,
    state_db_thread_refs: Vec<CodexStateDbThreadRef>,
    state_db_rollout_cwd_misses: usize,
    shell_snapshot_files: usize,
    shell_snapshot_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
    state_dbs_seen: usize,
}

#[derive(Default)]
struct CodexStateDbDiscovery {
    refs: Vec<CodexStateDbThreadRef>,
    sessions: Vec<CodexSession>,
    dbs_seen: usize,
    rollout_cwd_misses: usize,
}

struct CodexStateDbThreadRef {
    path: PathBuf,
    stored_cwd: String,
    count: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CodexDiscoveryMode {
    /// Command planners use native indexes so doctor output and apply atoms stay
    /// on the same path.
    Indexed,
    /// Reserved for whole-application health audits, where the user asked Babel
    /// to spend time finding stale or orphaned native state.
    Exhaustive,
}

struct CodexSession {
    id: String,
    path: PathBuf,
    selector: &'static str,
    action: &'static str,
    stored_cwd: String,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
    mode: CodexDiscoveryMode,
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, needles, mode)?;
    let mut state_roots = vec![
        context.codex_base(),
        context.codex_sessions(),
        context.codex_archived_sessions(),
        context.codex_base().join("history.jsonl"),
        context.codex_base().join("session_index.jsonl"),
        context.codex_base().join("config.toml"),
        context.codex_base().join("config.json"),
        context.codex_base().join("internal_storage.json"),
        context.codex_shell_snapshots(),
    ];
    state_roots.extend(
        discovery
            .state_db_thread_refs
            .iter()
            .map(|reference| reference.path.clone()),
    );
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    let mut session_cwd_groups =
        BTreeMap::<(&'static str, &'static str, String), (BTreeSet<PathBuf>, usize)>::new();
    for session in &discovery.matched_sessions {
        let entry = session_cwd_groups
            .entry((session.action, session.selector, session.stored_cwd.clone()))
            .or_default();
        entry.0.insert(session.path.clone());
        entry.1 += 1;
    }
    for ((action, selector, stored_cwd), (files, count)) in session_cwd_groups {
        edits.push(
            MigrationEdit::rewrite_jsonl_field_in_files(
                AgentKind::Codex,
                action,
                context.codex_sessions(),
                files.into_iter().collect(),
                selector,
                stored_cwd,
                new_path.display().to_string(),
                count,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.config_toml_ref_files > 0 {
        edits.push(
            MigrationEdit::rewrite_toml_table_key(
                AgentKind::Codex,
                "rewrite_project_config_keys",
                context.codex_base().join("config.toml"),
                "projects",
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.config_toml_ref_files,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.config_json_ref_files > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Codex,
                "rewrite_project_config_refs",
                context
                    .codex_base()
                    .join("config.json")
                    .display()
                    .to_string(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.config_json_ref_files,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.internal_storage_ref_files > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs(
                AgentKind::Codex,
                "rewrite_internal_storage_refs",
                context
                    .codex_base()
                    .join("internal_storage.json")
                    .display()
                    .to_string(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.internal_storage_ref_files,
            )
            .with_apply_ready(true),
        );
    }
    for reference in &discovery.state_db_thread_refs {
        edits.push(
            MigrationEdit::rewrite_sqlite_text_column(
                AgentKind::Codex,
                "rewrite_thread_index_cwd",
                reference.path.clone(),
                "threads",
                "cwd",
                reference.stored_cwd.clone(),
                new_path.display().to_string(),
                reference.count,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.session_path_ref_files > 0 {
        edits.push(
            MigrationEdit::rewrite_text_refs_in_files(
                AgentKind::Codex,
                "rewrite_session_path_refs",
                context.codex_sessions().display().to_string(),
                discovery.matched_session_path_ref_files.clone(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.session_path_ref_files,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.history_ref_entries > 0 {
        edits.push(
            MigrationEdit::rewrite_jsonl_field(
                AgentKind::Codex,
                "rewrite_history_path_refs",
                context.codex_base().join("history.jsonl"),
                "line containing source path",
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.history_ref_entries,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.session_index_ref_entries > 0 {
        edits.push(
            MigrationEdit::rewrite_jsonl_field(
                AgentKind::Codex,
                "rewrite_session_index_path_refs",
                context.codex_base().join("session_index.jsonl"),
                "line containing source path",
                old_path.display().to_string(),
                new_path.display().to_string(),
                discovery.session_index_ref_entries,
            )
            .with_apply_ready(true),
        );
    }
    if discovery.shell_snapshot_files > 0 {
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::Codex,
            "preserve_session_shell_snapshots",
            context.codex_shell_snapshots(),
            discovery.shell_snapshot_files,
            discovery.shell_snapshot_ref_files,
        ));
    }

    let mut notes = vec![
        "session identity fields: session_meta.payload.cwd and turn_context.payload.cwd"
            .to_string(),
    ];
    for root in [
        context.codex_sessions(),
        context.codex_archived_sessions(),
        context.codex_shell_snapshots(),
    ] {
        if !root.exists() {
            notes.push(format!("state root missing: {}", root.display()));
        }
    }
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} files; adapter needs a narrower pass before apply",
            discovery.files_scanned
        ));
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }
    if discovery.state_db_rollout_cwd_misses > 0 {
        notes.push(format!(
            "{} Codex thread row(s) matched cwd, but their rollout file had no matching cwd field",
            discovery.state_db_rollout_cwd_misses
        ));
    }
    let matched_session_ids = matched_session_ids(&discovery);
    if !matched_session_ids.is_empty() {
        let ids = matched_session_ids
            .iter()
            .take(3)
            .map(|(id, path)| format!("{} ({})", id, path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if matched_session_ids.len() > 3 {
            ", ..."
        } else {
            ""
        };
        notes.push(format!("matched Codex session id(s): {ids}{suffix}"));
    }

    let path_references_found = discovery.session_path_ref_files
        + discovery.history_ref_entries
        + discovery.session_index_ref_entries
        + discovery.config_toml_ref_files
        + discovery.config_json_ref_files
        + discovery.internal_storage_ref_files
        + discovery
            .state_db_thread_refs
            .iter()
            .map(|reference| reference.count)
            .sum::<usize>()
        + discovery.shell_snapshot_ref_files;

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Codex,
        AdapterReadiness::ApplyReady,
        state_roots,
        matched_session_ids.len(),
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    needles: &[String],
    mode: CodexDiscoveryMode,
) -> Result<CodexDiscovery> {
    tracing::debug!(
        old_path = %old_path.display(),
        needles = needles.len(),
        mode = ?mode,
        "mv.plan.codex: discovery starting"
    );
    let mut discovery = CodexDiscovery {
        session_roots: vec![context.codex_sessions(), context.codex_archived_sessions()],
        ..Default::default()
    };
    let child_prefix = format!("{}/", old_path.display());
    let state_db = collect_state_db_thread_refs(context, old_path, &child_prefix)?;
    discovery.state_dbs_seen = state_db.dbs_seen;
    discovery.state_db_thread_refs = state_db.refs;
    discovery.state_db_rollout_cwd_misses = state_db.rollout_cwd_misses;
    extend_unique_sessions(&mut discovery.matched_sessions, state_db.sessions);

    discovery.history_ref_entries =
        count_jsonl_line_refs(&context.codex_base().join("history.jsonl"), needles)?;
    discovery.session_index_ref_entries =
        count_jsonl_line_refs(&context.codex_base().join("session_index.jsonl"), needles)?;
    discovery.config_toml_ref_files =
        text_file_ref_count(&context.codex_base().join("config.toml"), needles)?;
    discovery.config_json_ref_files =
        text_file_ref_count(&context.codex_base().join("config.json"), needles)?;
    discovery.internal_storage_ref_files =
        text_file_ref_count(&context.codex_base().join("internal_storage.json"), needles)?;

    let db_found_cwd_without_rollout_paths =
        !discovery.state_db_thread_refs.is_empty() && discovery.matched_sessions.is_empty();
    let should_scan_rollouts = matches!(mode, CodexDiscoveryMode::Exhaustive)
        || discovery.state_dbs_seen == 0
        || db_found_cwd_without_rollout_paths;
    if should_scan_rollouts {
        let session_roots = discovery.session_roots.clone();
        for root in session_roots {
            collect_sessions_from_root(&root, old_path, &child_prefix, needles, &mut discovery)?;
        }
    } else {
        tracing::debug!(
            state_dbs_seen = discovery.state_dbs_seen,
            matched_sessions = discovery.matched_sessions.len(),
            "mv.plan.codex: skipping rollout tree scan; state DB is authoritative for indexed planning"
        );
    }

    collect_shell_snapshots(context, needles, &mut discovery)?;
    tracing::debug!(
        files_scanned = discovery.files_scanned,
        matched_sessions = discovery.matched_sessions.len(),
        session_path_ref_files = discovery.session_path_ref_files,
        history_ref_entries = discovery.history_ref_entries,
        session_index_ref_entries = discovery.session_index_ref_entries,
        state_db_refs = discovery.state_db_thread_refs.len(),
        state_db_rollout_cwd_misses = discovery.state_db_rollout_cwd_misses,
        state_dbs_seen = discovery.state_dbs_seen,
        shell_snapshot_files = discovery.shell_snapshot_files,
        truncated = discovery.truncated,
        "mv.plan.codex: discovery complete"
    );
    Ok(discovery)
}

fn collect_state_db_thread_refs(
    context: &HarnessOpsContext,
    old_path: &Path,
    child_prefix: &str,
) -> Result<CodexStateDbDiscovery> {
    let mut discovery = CodexStateDbDiscovery::default();
    for path in codex_state_dbs(context)? {
        discovery.dbs_seen += 1;
        let refs = collect_threads_cwd_refs(&path, old_path, child_prefix)?;
        discovery.refs.extend(refs.refs);
        discovery.sessions.extend(refs.sessions);
        discovery.rollout_cwd_misses += refs.rollout_cwd_misses;
    }
    Ok(discovery)
}

fn codex_state_dbs(context: &HarnessOpsContext) -> Result<Vec<PathBuf>> {
    let mut roots = codex_sqlite_roots(context);
    let mut paths = Vec::new();
    for root in roots.drain(..) {
        if !root.exists() {
            continue;
        }
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("state_") && name.ends_with(".sqlite") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn codex_sqlite_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let codex_base = context.codex_base();
    let mut roots = vec![codex_base.clone()];
    if let Some(root) = codex_sqlite_home_from_config(&codex_base.join("config.toml"), &codex_base)
    {
        roots.push(root);
    }
    if let Some(root) = context.codex_sqlite_home_env() {
        roots.push(normalize_codex_sqlite_home(root, &codex_base));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn codex_sqlite_home_from_config(config_path: &Path, codex_base: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(config_path).ok()?;
    let value = text.parse::<toml::Value>().ok()?;
    let sqlite_home = value.get("sqlite_home")?.as_str()?;
    Some(normalize_codex_sqlite_home(
        PathBuf::from(sqlite_home),
        codex_base,
    ))
}

fn normalize_codex_sqlite_home(path: PathBuf, codex_base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        codex_base.join(path)
    }
}

fn collect_threads_cwd_refs(
    path: &Path,
    old_path: &Path,
    child_prefix: &str,
) -> Result<CodexStateDbDiscovery> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='threads')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(CodexStateDbDiscovery::default());
    }

    let has_rollout_path: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads') WHERE name='rollout_path')",
        [],
        |row| row.get(0),
    )?;
    let mut discovery = CodexStateDbDiscovery::default();
    if !has_rollout_path {
        let mut statement = conn.prepare("SELECT cwd FROM threads")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut refs = BTreeMap::<String, usize>::new();
        for row in rows {
            let cwd = row?;
            if cwd_matches(Some(&cwd), old_path, child_prefix) {
                *refs.entry(cwd).or_default() += 1;
            }
        }
        discovery.refs = refs
            .into_iter()
            .map(|(stored_cwd, count)| CodexStateDbThreadRef {
                path: path.to_path_buf(),
                stored_cwd,
                count,
            })
            .collect();
        return Ok(discovery);
    }

    let mut statement = conn.prepare("SELECT id, rollout_path, cwd FROM threads")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut refs = BTreeMap::<String, usize>::new();
    for row in rows {
        let (id, rollout_path, cwd) = row?;
        if !cwd_matches(Some(&cwd), old_path, child_prefix) {
            continue;
        }
        *refs.entry(cwd.clone()).or_default() += 1;
        let rollout_path = PathBuf::from(rollout_path);
        let matches = read_session_cwd_matches(&rollout_path, old_path, child_prefix)
            .unwrap_or_else(|error| {
                tracing::debug!(
                    db = %path.display(),
                    rollout_path = %rollout_path.display(),
                    error = %error,
                    "mv.plan.codex: thread index rollout cwd probe failed"
                );
                Vec::new()
            });
        if matches.is_empty() {
            discovery.rollout_cwd_misses += 1;
        }
        for mut session in matches {
            if session.id == session_id_from_path(&rollout_path) {
                session.id = id.clone();
            }
            discovery.sessions.push(session);
        }
    }
    discovery.refs = refs
        .into_iter()
        .map(|(stored_cwd, count)| CodexStateDbThreadRef {
            path: path.to_path_buf(),
            stored_cwd,
            count,
        })
        .collect();
    Ok(discovery)
}

fn collect_sessions_from_root(
    root: &Path,
    old_path: &Path,
    child_prefix: &str,
    needles: &[String],
    discovery: &mut CodexDiscovery,
) -> Result<()> {
    if !root.exists() {
        tracing::debug!(
            root = %root.display(),
            "mv.plan.codex: session root missing"
        );
        return Ok(());
    }

    let before_scanned = discovery.files_scanned;
    let before_matches = discovery.session_path_ref_files;
    tracing::debug!(
        root = %root.display(),
        "mv.plan.codex: session root scan starting"
    );
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if discovery.files_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
            continue;
        }
        if !metadata.is_file() || !is_session_file(&path) {
            continue;
        }

        discovery.files_scanned += 1;
        if discovery.files_scanned % 500 == 0 {
            tracing::debug!(
                root = %root.display(),
                files_scanned = discovery.files_scanned,
                session_path_ref_files = discovery.session_path_ref_files,
                matched_sessions = discovery.matched_sessions.len(),
                "mv.plan.codex: session root scan progress"
            );
        }
        if text_file_contains_any(&path, metadata.len(), needles)? {
            discovery.session_path_ref_files += 1;
            discovery.matched_session_path_ref_files.push(path.clone());
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }

        discovery
            .matched_sessions
            .extend(read_session_cwd_matches(&path, old_path, child_prefix)?);
    }
    tracing::debug!(
        root = %root.display(),
        files_scanned = discovery.files_scanned - before_scanned,
        path_ref_files = discovery.session_path_ref_files - before_matches,
        truncated = discovery.truncated,
        "mv.plan.codex: session root scan complete"
    );
    Ok(())
}

fn extend_unique_sessions(target: &mut Vec<CodexSession>, sessions: Vec<CodexSession>) {
    let mut seen = target
        .iter()
        .map(|session| {
            (
                session.path.clone(),
                session.selector,
                session.stored_cwd.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    for session in sessions {
        if seen.insert((
            session.path.clone(),
            session.selector,
            session.stored_cwd.clone(),
        )) {
            target.push(session);
        }
    }
}

fn matched_session_ids(discovery: &CodexDiscovery) -> BTreeMap<String, PathBuf> {
    discovery
        .matched_sessions
        .iter()
        .map(|session| (session.id.clone(), session.path.clone()))
        .collect()
}

fn read_session_cwd_matches(
    path: &Path,
    old_path: &Path,
    child_prefix: &str,
) -> Result<Vec<CodexSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut matches = Vec::new();
    for line in reader.lines().take(150) {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some((selector, action)) = codex_cwd_selector(kind) else {
            continue;
        };
        let payload = value.get("payload").unwrap_or(&serde_json::Value::Null);
        let cwd = payload.get("cwd").and_then(|value| value.as_str());
        if !cwd_matches(cwd, old_path, child_prefix) {
            continue;
        }
        let id = payload
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| session_id_from_path(path));
        matches.push(CodexSession {
            id,
            path: path.to_path_buf(),
            selector,
            action,
            stored_cwd: cwd.unwrap_or_default().to_string(),
        });
    }

    Ok(matches)
}

fn codex_cwd_selector(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "session_meta" => Some((
            "$.payload.cwd where $.type == \"session_meta\"",
            "rewrite_session_meta_cwd",
        )),
        "turn_context" => Some((
            "$.payload.cwd where $.type == \"turn_context\"",
            "rewrite_turn_context_cwd",
        )),
        _ => None,
    }
}

fn collect_shell_snapshots(
    context: &HarnessOpsContext,
    needles: &[String],
    discovery: &mut CodexDiscovery,
) -> Result<()> {
    let root = context.codex_shell_snapshots();
    if !root.exists() || discovery.matched_sessions.is_empty() {
        return Ok(());
    }

    let session_ids = discovery
        .matched_sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<BTreeSet<_>>();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((session_id, _)) = name.split_once('.') else {
            continue;
        };
        if !session_ids.contains(session_id) {
            continue;
        }

        discovery.shell_snapshot_files += 1;
        if text_file_contains_any(&path, metadata.len(), needles)? {
            discovery.shell_snapshot_ref_files += 1;
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }
    }
    Ok(())
}

fn count_jsonl_line_refs(path: &Path, needles: &[String]) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut count = 0;
    for line in reader.lines() {
        let line = line?;
        if needles.iter().any(|needle| line.contains(needle)) {
            count += 1;
        }
    }
    Ok(count)
}

fn text_file_ref_count(path: &Path, needles: &[String]) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !is_probably_text_state_file(path) {
        return Ok(0);
    }
    Ok(usize::from(text_file_contains_any(
        path,
        metadata.len(),
        needles,
    )?))
}

fn cwd_matches(cwd: Option<&str>, old_path: &Path, child_prefix: &str) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let old = old_path.to_string_lossy();
    if cwd == old.as_ref() || cwd.starts_with(child_prefix) {
        return true;
    }

    let normalized_cwd = normalize_lexical_path(Path::new(cwd));
    let normalized_old = normalize_lexical_path(old_path);
    normalized_cwd == normalized_old || normalized_cwd.starts_with(&normalized_old)
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown");
    if let Some(rest) = stem.strip_prefix("rollout-") {
        // rollout-YYYY-MM-DDTHH-MM-SS-<session-id>
        if rest.len() > 20 && rest.as_bytes().get(19) == Some(&b'-') {
            return rest[20..].to_string();
        }
    }
    stem.to_string()
}

fn is_session_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("jsonl") | Some("json")
    )
}
