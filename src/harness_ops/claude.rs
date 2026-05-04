use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    for_each_jsonl_value, scan_text_refs, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, MigrationEdit, MigrationRisk, RecoveryClass, RiskSeverity,
};

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
    risks: &mut Vec<MigrationRisk>,
) -> Result<HarnessMigrationReport> {
    let projects_dir = context.claude_base().join("projects");
    let source_candidates = claude_project_candidates(&projects_dir, old_path);
    let dest_candidates = claude_project_candidates(&projects_dir, new_path);
    let history_path = context.claude_base().join("history.jsonl");
    let session_keyed_roots = claude_session_keyed_roots(context);
    let user_wide_files = claude_user_wide_files(context);

    let existing_sources: Vec<_> = source_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
        .collect();
    let sessions_found = existing_sources.iter().try_fold(0, |count, candidate| {
        Ok::<_, anyhow::Error>(count + count_jsonl_files(&candidate.path)?)
    })?;
    let history_refs = count_history_refs(&history_path, old_path)?;
    let session_root_scans = session_keyed_roots
        .iter()
        .map(|root| Ok((root.clone(), scan_text_refs(root, needles)?)))
        .collect::<Result<Vec<_>>>()?;
    let user_wide_scans = user_wide_files
        .iter()
        .map(|file| Ok((file.clone(), scan_text_refs(file, needles)?)))
        .collect::<Result<Vec<_>>>()?;
    let session_refs = session_root_scans
        .iter()
        .map(|(_, scan)| scan.path_references_found)
        .sum::<usize>();
    let user_wide_refs = user_wide_scans
        .iter()
        .map(|(_, scan)| scan.path_references_found)
        .sum::<usize>();
    let mut edits = Vec::new();
    let mut notes = Vec::new();

    for source in &existing_sources {
        let Some(dest) = dest_candidates
            .iter()
            .find(|candidate| candidate.scheme == source.scheme)
            .or_else(|| dest_candidates.first())
        else {
            continue;
        };
        edits.push(
            MigrationEdit::rename_path(
                AgentKind::Claude,
                "rename_project_dir",
                source.path.clone(),
                dest.path.clone(),
                format!("preserve {} Claude transcript file(s)", sessions_found),
            )
            .with_apply_ready(true)
            .with_recovery(RecoveryClass::SessionDependencyDir),
        );
    }

    let existing_destinations: Vec<_> = dest_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
        .collect();

    if existing_sources.is_empty() {
        notes.push(format!(
            "Claude project directory not found; probed keys: {}",
            source_candidates
                .iter()
                .map(|candidate| candidate.encoded.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !existing_destinations.is_empty() {
            notes.push(format!(
                "Claude destination project key already exists; treating project rename as already applied: {}",
                existing_destinations
                    .iter()
                    .map(|candidate| candidate.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    if history_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_jsonl_field(
                AgentKind::Claude,
                "rewrite_history_paths",
                history_path.clone(),
                "$.project",
                old_path.display().to_string(),
                new_path.display().to_string(),
                history_refs,
            )
            .with_apply_ready(true)
            .with_recovery(RecoveryClass::SessionDependencyFile),
        );
    }

    for (root, scan) in &session_root_scans {
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Claude,
                    "rewrite_session_keyed_refs",
                    root.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true)
                .with_recovery(RecoveryClass::SessionDependencyDir),
            );
        }
    }

    for (file, scan) in &user_wide_scans {
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Claude,
                    "rewrite_user_wide_refs",
                    file.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true)
                .with_recovery(RecoveryClass::SessionDependencyFile),
            );
        }
    }

    for source in &existing_sources {
        let Some(dest) = dest_candidates
            .iter()
            .find(|candidate| candidate.scheme == source.scheme)
            .or_else(|| dest_candidates.first())
        else {
            continue;
        };
        if dest.path.exists() {
            risks.push(MigrationRisk {
                severity: RiskSeverity::Blocker,
                harness: Some(AgentKind::Claude),
                message: format!(
                    "Claude destination project folder already exists for {} key: {}",
                    dest.scheme,
                    dest.path.display()
                ),
            });
        }
    }

    if source_candidates
        .iter()
        .zip(dest_candidates.iter())
        .any(|(old, new)| old.encoded == new.encoded)
    {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: Some(AgentKind::Claude),
            message: "Claude source and destination can encode to the same project key".to_string(),
        });
    }

    let mut state_roots = vec![context.claude_base(), context.home.join(".claude.json")];
    state_roots.extend(session_keyed_roots);
    state_roots.extend(user_wide_files);
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Claude,
        AdapterReadiness::ApplyReady,
        state_roots,
        sessions_found,
        history_refs + session_refs + user_wide_refs,
        edits,
        notes,
    ))
}

#[derive(Debug)]
struct ClaudeProjectCandidate {
    scheme: &'static str,
    encoded: String,
    path: PathBuf,
}

fn claude_project_candidates(
    projects_dir: &Path,
    project_path: &Path,
) -> Vec<ClaudeProjectCandidate> {
    let mut candidates = Vec::new();
    let cc_port = claude_encode_cc_port(project_path);
    candidates.push(ClaudeProjectCandidate {
        scheme: "primary",
        path: projects_dir.join(&cc_port),
        encoded: cc_port,
    });

    let ccmv = claude_encode_ccmv(project_path);
    if candidates.iter().all(|candidate| candidate.encoded != ccmv) {
        candidates.push(ClaudeProjectCandidate {
            scheme: "compat",
            path: projects_dir.join(&ccmv),
            encoded: ccmv,
        });
    }

    candidates
}

pub(super) fn claude_encode_cc_port(path: &Path) -> String {
    normalized_path_for_key(path)
        .replace('/', "-")
        .replace('.', "-")
        .replace(' ', "-")
}

pub(super) fn claude_encode_ccmv(path: &Path) -> String {
    let normalized = normalized_path_for_key(path);
    normalized
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn normalized_path_for_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.len() > 1 {
        raw.trim_end_matches('/').to_string()
    } else {
        raw.to_string()
    }
}

fn claude_session_keyed_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.claude_base().join("todos"),
        context.claude_base().join("usage-data/session-meta"),
        context.claude_base().join("usage-data/facets"),
        context.claude_base().join("plugins/data"),
        context.claude_base().join("tasks"),
    ]
}

fn claude_user_wide_files(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(".claude.json"),
        context.claude_base().join("settings.json"),
        context.claude_base().join("plugins/installed_plugins.json"),
        context
            .claude_base()
            .join("plugins/known_marketplaces.json"),
    ]
}

fn count_jsonl_files(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            count += 1;
        }
    }
    Ok(count)
}

pub(super) fn count_history_refs(history_path: &Path, old_path: &Path) -> Result<usize> {
    let old_path = old_path.to_string_lossy();
    let child_prefix = format!("{}/", old_path);
    let mut count = 0;
    for_each_jsonl_value::<()>(history_path, None, |value| {
        if let Some(project) = value.get("project").and_then(|v| v.as_str()) {
            if project == old_path.as_ref() || project.starts_with(&child_prefix) {
                count += 1;
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(count)
}
