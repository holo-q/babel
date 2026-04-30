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
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, needles)?;
    let mut notes = vec![
        "Roo Code is a Cline-family VS Code extension; references agree on globalStorage/<extension>/tasks/<task_id> with ui_messages.json plus api_conversation_history.json/task_metadata.json sidecars.".to_string(),
        "This migration doctor preserves extension storage only and never claims live Roo task control.".to_string(),
        "Semantics consumed: references/mnemo indexes rooveterinaryinc.roo-cline with the shared Cline/Roo/Kilo task parser; coding_agent_session_search probes the same Code/Cursor globalStorage roots.".to_string(),
    ];

    if discovery.existing_roots.is_empty() {
        notes.push("no Roo Code state roots detected".to_string());
    }
    if !discovery.probed_roots.is_empty() {
        notes.push(format!(
            "probed Roo roots: {}",
            discovery
                .probed_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if discovery.truncated {
        notes.push(
            "path-reference scan hit the shared scan cap; keep Roo apply disabled until a Roo-specific rewrite fixture exists"
                .to_string(),
        );
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large Roo state file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }

    let edits = discovery
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

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::RooCode,
        AdapterReadiness::DoctorOnly,
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
    fn roo_planner_counts_task_storage_and_keeps_apply_disabled() {
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
        let report = plan(&context, &[old_path.display().to_string()]).unwrap();

        assert_eq!(report.harness, AgentKind::RooCode);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 1);
        assert_eq!(report.edits.len(), 1);
        assert!(report.edits.iter().all(|edit| !edit.apply_ready));
    }

    #[test]
    fn roo_planner_reports_probed_roots_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessOpsContext::from_home(tmp.path().to_path_buf());
        let report = plan(&context, &[]).unwrap();

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
