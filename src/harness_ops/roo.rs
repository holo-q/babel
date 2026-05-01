use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit, TextScan};

const ROO_EXTENSION_ID: &str = "rooveterinaryinc.roo-cline";
const ROO_TRANSCRIPT_FILES: &[&str] = &[
    "ui_messages.json",
    "api_conversation_history.json",
    "task_metadata.json",
];
const VSCODE_FAMILY_APPS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "Cursor",
    "Windsurf",
    "VSCodium",
    "Antigravity",
    "Kiro",
    "Trae",
];

#[derive(Debug, Default)]
struct RooDiscovery {
    probed_roots: Vec<PathBuf>,
    existing_roots: Vec<PathBuf>,
    task_roots: Vec<PathBuf>,
    sessions_found: usize,
    path_references_found: usize,
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
    let mut notes = vec![
        "storage: VS Code-family globalStorage/rooveterinaryinc.roo-cline/tasks/<taskId>"
            .to_string(),
    ];

    if discovery.existing_roots.is_empty() {
        notes.push("no Roo Code state roots detected".to_string());
    }
    if !discovery.probed_roots.is_empty() {
        notes.push(format!(
            "probed {} Roo root candidate(s)",
            discovery.probed_roots.len()
        ));
    }
    if discovery.truncated {
        notes.push("path-reference scan hit the shared scan cap".to_string());
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large Roo state file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }

    let mut edits: Vec<_> = discovery
        .task_roots
        .iter()
        .map(|root| {
            MigrationEdit::preserve_session_keyed_files(
                AgentKind::RooCode,
                "preserve_roo_tasks",
                root.clone(),
                count_task_sessions(root).unwrap_or(0),
                count_task_path_refs(root, needles).unwrap_or(0),
            )
        })
        .collect();
    for root in &discovery.existing_roots {
        let scan = super::scan_text_refs(root, needles)?;
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::RooCode,
                    "rewrite_roo_path_refs",
                    root.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true),
            );
        }
    }

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::RooCode,
        AdapterReadiness::ApplyReady,
        discovery.existing_roots,
        discovery.sessions_found,
        discovery.path_references_found,
        edits,
        notes,
    ))
}

fn discover(context: &HarnessOpsContext, needles: &[String]) -> Result<RooDiscovery> {
    let mut discovery = RooDiscovery::default();

    for extension_root in roo_extension_roots(context) {
        discovery.probed_roots.push(extension_root.clone());
        if !extension_root.exists() {
            continue;
        }

        let scan = super::scan_text_refs(&extension_root, needles)?;
        discovery.path_references_found += scan.path_references_found;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        discovery.existing_roots.push(extension_root.clone());

        let tasks_root = extension_root.join("tasks");
        if tasks_root.exists() {
            discovery.sessions_found += count_task_sessions(&tasks_root)?;
            discovery.task_roots.push(tasks_root);
        }
    }

    discovery.existing_roots.sort();
    discovery.existing_roots.dedup();
    discovery.task_roots.sort();
    discovery.task_roots.dedup();

    Ok(discovery)
}

fn roo_extension_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    VSCODE_FAMILY_APPS
        .iter()
        .map(|app| {
            context
                .home
                .join(".config")
                .join(app)
                .join("User/globalStorage")
                .join(ROO_EXTENSION_ID)
        })
        .collect()
}

fn count_task_sessions(tasks_root: &Path) -> Result<usize> {
    if !tasks_root.exists() {
        return Ok(0);
    }

    let mut count = 0;
    for entry in fs::read_dir(tasks_root)? {
        let entry = entry?;
        let task_path = entry.path();
        if !task_path.is_dir() {
            continue;
        }
        if ROO_TRANSCRIPT_FILES
            .iter()
            .any(|file_name| task_path.join(file_name).exists())
        {
            count += 1;
        }
    }
    Ok(count)
}

fn count_task_path_refs(tasks_root: &Path, needles: &[String]) -> Result<usize> {
    let scan: TextScan = super::scan_text_refs(tasks_root, needles)?;
    Ok(scan.path_references_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roo_planner_counts_task_storage_and_marks_concrete_refs_apply_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old_path = home.join("Workspace/old");
        let root = home
            .join(".config/Code/User/globalStorage")
            .join(ROO_EXTENSION_ID);
        let task = root.join("tasks/task-1");
        fs::create_dir_all(&task).unwrap();
        fs::write(
            task.join("ui_messages.json"),
            format!(
                r#"[{{"type":"say","say":"text","text":"work in {}"}}]"#,
                old_path.display()
            ),
        )
        .unwrap();
        fs::write(task.join("task_metadata.json"), r#"{"id":"task-1"}"#).unwrap();

        let context = HarnessOpsContext::from_home(home.to_path_buf());
        let new_path = home.join("Workspace/new");
        let report = plan(
            &context,
            &old_path,
            &new_path,
            &[old_path.display().to_string()],
        )
        .unwrap();

        assert_eq!(report.harness, AgentKind::RooCode);
        assert!(matches!(report.readiness, AdapterReadiness::ApplyReady));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 1);
        assert!(report.edits.iter().any(|edit| edit.apply_ready));
    }

    #[test]
    fn roo_planner_reports_probed_roots_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessOpsContext::from_home(tmp.path().to_path_buf());
        let old_path = tmp.path().join("old");
        let new_path = tmp.path().join("new");
        let report = plan(&context, &old_path, &new_path, &[]).unwrap();

        assert_eq!(report.sessions_found, 0);
        assert!(report.state_roots.is_empty());
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("no Roo Code state roots detected")));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("rooveterinaryinc.roo-cline")));
    }
}
