use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    scan_text_refs, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit,
};

#[derive(Default)]
struct KiroDiscovery {
    roots: Vec<PathBuf>,
    ide_workspace_refs: usize,
    acp_session_refs: usize,
    matched_workspace_sessions: usize,
    matched_acp_files: usize,
    truncated: bool,
    large_files_sampled: usize,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, needles)?;
    let mut edits = Vec::new();

    if discovery.ide_workspace_refs > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Kiro,
            "rewrite_ide_workspace_session_refs",
            "Kiro IDE workspace-sessions JSON",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.ide_workspace_refs,
        ));
    }
    if discovery.acp_session_refs > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Kiro,
            "rewrite_acp_session_refs",
            "~/.kiro/sessions/cli JSON/JSONL",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.acp_session_refs,
        ));
    }

    let mut notes = vec![
        "storage candidates: IDE workspace JSON, legacy workspace-sessions, and ~/.kiro/sessions/cli".to_string(),
    ];
    if discovery.roots.is_empty() {
        notes.push("no known Kiro state roots detected".to_string());
    }
    if discovery.truncated {
        notes.push(
            "one or more Kiro scans hit the shared file cap; narrow before apply".to_string(),
        );
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large Kiro file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }
    if discovery.matched_workspace_sessions > 0 {
        notes.push(format!(
            "matched {} Kiro IDE workspace session file(s) containing source path references",
            discovery.matched_workspace_sessions
        ));
    }
    if discovery.matched_acp_files > 0 {
        notes.push(format!(
            "matched {} Kiro ACP session file(s) containing source path references",
            discovery.matched_acp_files
        ));
    }

    let sessions_found = discovery.matched_workspace_sessions + discovery.matched_acp_files;
    let path_references_found = discovery.ide_workspace_refs + discovery.acp_session_refs;
    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Kiro,
        AdapterReadiness::DoctorOnly,
        discovery.roots,
        sessions_found,
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(context: &HarnessOpsContext, needles: &[String]) -> Result<KiroDiscovery> {
    let mut discovery = KiroDiscovery::default();

    for root in kiro_ide_workspace_roots(context) {
        if !root.exists() {
            continue;
        }
        let scan = scan_text_refs(&root, needles)?;
        discovery.ide_workspace_refs += scan.path_references_found;
        discovery.matched_workspace_sessions += scan.path_references_found;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        discovery.roots.push(root);
    }

    let acp_root = context.home.join(".kiro/sessions/cli");
    if acp_root.exists() {
        let scan = scan_text_refs(&acp_root, needles)?;
        discovery.acp_session_refs += scan.path_references_found;
        discovery.matched_acp_files += scan.path_references_found;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        discovery.roots.push(acp_root);
    }

    let cli_root = context.home.join(".kiro");
    if cli_root.exists() && !discovery.roots.iter().any(|root| root == &cli_root) {
        discovery.roots.push(cli_root);
    }
    discovery.roots.sort();
    discovery.roots.dedup();
    Ok(discovery)
}

fn kiro_ide_workspace_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let relative = [
        "User",
        "globalStorage",
        "kiro.kiroagent",
        "workspace-sessions",
    ];
    let mut roots = Vec::new();

    roots.push(join_segments(
        context.home.join("Library/Application Support/Kiro"),
        &relative,
    ));
    roots.push(join_segments(context.home.join(".config/Kiro"), &relative));
    roots.push(join_segments(
        context.home.join("AppData/Roaming/Kiro"),
        &relative,
    ));
    roots.push(
        context
            .home
            .join("Library/Application Support/Kiro/workspace-sessions"),
    );

    roots
}

fn join_segments(mut root: PathBuf, segments: &[&str]) -> PathBuf {
    for segment in segments {
        root.push(segment);
    }
    root
}
