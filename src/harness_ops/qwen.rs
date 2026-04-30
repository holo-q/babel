use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    text_file_contains_any, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext,
    MigrationEdit, MAX_SCAN_BYTES, MAX_SCAN_FILES,
};

#[derive(Default)]
struct QwenDiscovery {
    session_files: usize,
    session_path_ref_files: usize,
    config_ref_files: usize,
    other_path_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let roots = qwen_state_roots(context);
    let discovery = discover(&[context.qwen_base()], needles)?;

    let mut edits = Vec::new();
    if discovery.config_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::QwenCode,
            "rewrite_project_config_workspace_refs",
            "~/.qwen/{projects,tmp,history}/*/config.json",
            "",
            "",
            discovery.config_ref_files,
        ));
    }
    if discovery.session_path_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_jsonl_field(
            AgentKind::QwenCode,
            "rewrite_chat_record_cwd_refs",
            context.qwen_base(),
            "$.cwd and path-bearing chat records under {projects,tmp,history}/*/chats",
            "",
            "",
            discovery.session_path_ref_files,
        ));
    }
    if discovery.other_path_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::QwenCode,
            "rewrite_auxiliary_state_refs",
            "~/.qwen/{projects,tmp,history}",
            "",
            "",
            discovery.other_path_ref_files,
        ));
    }
    if discovery.session_files > 0 {
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::QwenCode,
            "preserve_qwen_chat_trees",
            context.qwen_base(),
            discovery.session_files,
            discovery.session_path_ref_files,
        ));
    }

    let mut existing_roots: Vec<_> = roots.into_iter().filter(|root| root.exists()).collect();
    existing_roots.sort();
    existing_roots.dedup();

    let mut notes = vec![
        "Qwen runtime base defaults to ~/.qwen; cli-continues documents QWEN_RUNTIME_DIR as the canonical upstream override, but Babel's context currently only supplies home-rooted planning.".to_string(),
        "References used: coding_agent_session_search/tests/connector_qwen.rs fixtures cover ~/.qwen/tmp/<project>/config.json plus chats/session-*.json; cli-continues covers ~/.qwen/projects/<sanitized-cwd>/chats/*.jsonl and the evolving tmp/history roots.".to_string(),
        "Qwen chat transcripts are tree-structured JSONL via uuid/parentUuid, so doctor preserves chat trees separately from path-bearing config/workspace references.".to_string(),
    ];
    if existing_roots.is_empty() {
        notes.push("no known Qwen state roots detected".to_string());
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

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::QwenCode,
        AdapterReadiness::DoctorOnly,
        existing_roots,
        discovery.session_files,
        discovery.config_ref_files
            + discovery.session_path_ref_files
            + discovery.other_path_ref_files,
        edits,
        notes,
    ))
}

fn qwen_state_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let base = context.qwen_base();
    vec![
        base.clone(),
        base.join("projects"),
        base.join("tmp"),
        base.join("history"),
    ]
}

fn discover(roots: &[PathBuf], needles: &[String]) -> Result<QwenDiscovery> {
    let mut discovery = QwenDiscovery::default();

    for root in roots {
        collect_root(root, needles, &mut discovery)?;
        if discovery.truncated {
            break;
        }
    }

    Ok(discovery)
}

fn collect_root(root: &Path, needles: &[String], discovery: &mut QwenDiscovery) -> Result<()> {
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
        if !metadata.is_file() || !is_qwen_state_file(&path) {
            continue;
        }

        discovery.files_scanned += 1;
        let contains_source_path = text_file_contains_any(&path, metadata.len(), needles)?;
        if metadata.len() > MAX_SCAN_BYTES {
            discovery.large_files_sampled += 1;
        }

        if is_qwen_session_file(&path) {
            discovery.session_files += 1;
            if contains_source_path {
                discovery.session_path_ref_files += 1;
            }
        } else if is_project_config(&path) {
            if contains_source_path {
                discovery.config_ref_files += 1;
            }
        } else if contains_source_path {
            discovery.other_path_ref_files += 1;
        }
    }
    Ok(())
}

fn is_qwen_state_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl")
    )
}

fn is_qwen_session_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let in_chats_dir = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("chats");

    in_chats_dir
        && (name.ends_with(".jsonl") || (name.starts_with("session-") && name.ends_with(".json")))
}

fn is_project_config(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("config.json")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn qwen_reports_native_roots_and_path_bound_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessOpsContext::from_home(tmp.path().to_path_buf());
        let qwen = context.qwen_base();
        write_file(
            &qwen.join("tmp/project-hash/config.json"),
            r#"{"workspace":"/workspace/qwen-real-service"}"#,
        );
        write_file(
            &qwen.join("tmp/project-hash/chats/session-1731107950138-qwen.json"),
            r#"{"sessionId":"qwen-session-1","messages":[{"type":"user","content":"hi"}]}"#,
        );
        write_file(
            &qwen.join("projects/sanitized/chats/qwen-session-2.jsonl"),
            r#"{"uuid":"u1","sessionId":"qwen-session-2","type":"user","cwd":"/workspace/qwen-real-service"}"#,
        );

        let report = plan(&context, &["/workspace/qwen-real-service".to_string()]).unwrap();

        assert_eq!(report.harness, AgentKind::QwenCode);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 2);
        assert_eq!(report.path_references_found, 2);
        assert!(report
            .state_roots
            .iter()
            .any(|path| path.ends_with(".qwen/tmp")));
        assert!(report
            .operations
            .iter()
            .all(|operation| !operation.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|operation| operation.action == "rewrite_project_config_workspace_refs"));
        assert!(report
            .operations
            .iter()
            .any(|operation| operation.action == "rewrite_chat_record_cwd_refs"));
        assert!(report
            .operations
            .iter()
            .any(|operation| operation.action == "preserve_qwen_chat_trees"));
    }
}
