use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    is_probably_text_state_file, text_file_contains_any, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, PlannedOperation, MAX_SCAN_BYTES, MAX_SCAN_FILES,
};

#[derive(Default)]
struct CodexDiscovery {
    session_roots: Vec<PathBuf>,
    matched_sessions: Vec<CodexSession>,
    session_path_ref_files: usize,
    history_ref_entries: usize,
    session_index_ref_entries: usize,
    project_config_ref_files: usize,
    shell_snapshot_files: usize,
    shell_snapshot_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

struct CodexSession {
    id: String,
    path: PathBuf,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, needles)?;
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
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut operations = Vec::new();
    if !discovery.matched_sessions.is_empty() {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "rewrite_session_meta_cwd".to_string(),
            target: context.codex_sessions().display().to_string(),
            detail: format!(
                "{} session(s) have cwd under source; set cwd to {}",
                discovery.matched_sessions.len(),
                new_path.display()
            ),
            apply_ready: false,
        });
    }
    if discovery.project_config_ref_files > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "rewrite_project_config_keys".to_string(),
            target: context.codex_base().display().to_string(),
            detail: format!(
                "{} Codex config file(s) contain trusted project entries or path refs",
                discovery.project_config_ref_files
            ),
            apply_ready: false,
        });
    }
    if discovery.session_path_ref_files > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "rewrite_session_path_refs".to_string(),
            target: context.codex_sessions().display().to_string(),
            detail: format!(
                "{} session file(s) contain source path references",
                discovery.session_path_ref_files
            ),
            apply_ready: false,
        });
    }
    if discovery.history_ref_entries > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "rewrite_history_path_refs".to_string(),
            target: context
                .codex_base()
                .join("history.jsonl")
                .display()
                .to_string(),
            detail: format!(
                "{} history entrie(s) contain source path references",
                discovery.history_ref_entries
            ),
            apply_ready: false,
        });
    }
    if discovery.session_index_ref_entries > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "rewrite_session_index_path_refs".to_string(),
            target: context
                .codex_base()
                .join("session_index.jsonl")
                .display()
                .to_string(),
            detail: format!(
                "{} session-index entrie(s) contain source path references",
                discovery.session_index_ref_entries
            ),
            apply_ready: false,
        });
    }
    if discovery.shell_snapshot_files > 0 {
        operations.push(PlannedOperation {
            harness: AgentKind::Codex,
            action: "preserve_session_shell_snapshots".to_string(),
            target: context.codex_shell_snapshots().display().to_string(),
            detail: format!(
                "{} shell snapshot file(s) are keyed by matched session id(s); {} contain source path refs",
                discovery.shell_snapshot_files, discovery.shell_snapshot_ref_files
            ),
            apply_ready: false,
        });
    }

    let mut notes = vec![
        "Codex identity is session_meta.payload.cwd in rollout JSONL; path-bearing transcripts are reported separately.".to_string(),
        "References used: cdxresume and cli-continues parse Codex 0.32+ rollout files from ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl.".to_string(),
        "Codex apply is still disabled until rewrite/backup/rollback fixtures exist for session_meta, config.toml project keys, history, and shell snapshots.".to_string(),
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
    if !discovery.matched_sessions.is_empty() {
        let ids = discovery
            .matched_sessions
            .iter()
            .take(3)
            .map(|session| format!("{} ({})", session.id, session.path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if discovery.matched_sessions.len() > 3 {
            ", ..."
        } else {
            ""
        };
        notes.push(format!("matched Codex session id(s): {ids}{suffix}"));
    }

    let path_references_found = discovery.session_path_ref_files
        + discovery.history_ref_entries
        + discovery.session_index_ref_entries
        + discovery.project_config_ref_files
        + discovery.shell_snapshot_ref_files;

    Ok(HarnessMigrationReport {
        harness: AgentKind::Codex,
        readiness: AdapterReadiness::DoctorOnly,
        state_roots,
        sessions_found: discovery.matched_sessions.len(),
        path_references_found,
        operations,
        notes,
    })
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    needles: &[String],
) -> Result<CodexDiscovery> {
    let mut discovery = CodexDiscovery {
        session_roots: vec![context.codex_sessions(), context.codex_archived_sessions()],
        ..Default::default()
    };
    let child_prefix = format!("{}/", old_path.display());
    let session_roots = discovery.session_roots.clone();

    for root in session_roots {
        collect_sessions_from_root(&root, old_path, &child_prefix, needles, &mut discovery)?;
    }

    discovery.history_ref_entries =
        count_jsonl_line_refs(&context.codex_base().join("history.jsonl"), needles)?;
    discovery.session_index_ref_entries =
        count_jsonl_line_refs(&context.codex_base().join("session_index.jsonl"), needles)?;
    discovery.project_config_ref_files = [
        context.codex_base().join("config.toml"),
        context.codex_base().join("config.json"),
        context.codex_base().join("internal_storage.json"),
    ]
    .into_iter()
    .map(|path| text_file_ref_count(&path, needles))
    .collect::<Result<Vec<_>>>()?
    .into_iter()
    .sum();

    collect_shell_snapshots(context, needles, &mut discovery)?;
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
        return Ok(());
    }

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
        if text_file_contains_any(&path, metadata.len(), needles)? {
            discovery.session_path_ref_files += 1;
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }

        if let Some(session) = read_session_identity(&path, old_path, child_prefix)? {
            discovery.matched_sessions.push(session);
        }
    }
    Ok(())
}

fn read_session_identity(
    path: &Path,
    old_path: &Path,
    child_prefix: &str,
) -> Result<Option<CodexSession>> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line)? == 0 {
        return Ok(None);
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(first_line.trim()) else {
        return Ok(None);
    };

    if value.get("type").and_then(|value| value.as_str()) == Some("session_meta") {
        let payload = value.get("payload").unwrap_or(&serde_json::Value::Null);
        let cwd = payload.get("cwd").and_then(|value| value.as_str());
        if !cwd_matches(cwd, old_path, child_prefix) {
            return Ok(None);
        }
        let id = payload
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| session_id_from_path(path));
        return Ok(Some(CodexSession {
            id,
            path: path.to_path_buf(),
        }));
    }

    // Pre-rollout Codex did not put cwd in a dedicated session_meta record.
    // cdxresume recovers it from the initial environment context, so Babel uses
    // the same witness before claiming a legacy file belongs to this project.
    if legacy_file_mentions_cwd(path, old_path, child_prefix)? {
        let id = value
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| session_id_from_path(path));
        return Ok(Some(CodexSession {
            id,
            path: path.to_path_buf(),
        }));
    }

    Ok(None)
}

fn legacy_file_mentions_cwd(path: &Path, old_path: &Path, child_prefix: &str) -> Result<bool> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(150) {
        let line = line?;
        let Some(start) = line.find("<cwd>") else {
            continue;
        };
        let rest = &line[start + "<cwd>".len()..];
        let Some(end) = rest.find("</cwd>") else {
            continue;
        };
        let cwd = &rest[..end];
        if cwd == old_path.to_string_lossy() || cwd.starts_with(child_prefix) {
            return Ok(true);
        }
    }
    Ok(false)
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
    cwd == old.as_ref() || cwd.starts_with(child_prefix)
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
