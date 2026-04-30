use std::fs;
use std::path::{Path, PathBuf};

use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, MigrationEdit};

const AIDER_CHAT_HISTORY: &str = ".aider.chat.history.md";

#[cfg(test)]
pub(super) fn plan() -> HarnessMigrationReport {
    plan_with_project_files(Vec::new(), false)
}

pub(super) fn plan_for_source(old_path: &Path) -> HarnessMigrationReport {
    let project_files = discover_project_local_files(old_path);
    plan_with_project_files(project_files, true)
}

fn plan_with_project_files(
    project_files: Vec<PathBuf>,
    source_path_available: bool,
) -> HarnessMigrationReport {
    let target = if source_path_available {
        if project_files.is_empty() {
            "source directory .aider* files".to_string()
        } else {
            project_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    } else {
        "source directory .aider* files (source path unavailable)".to_string()
    };

    let detail = if source_path_available {
        "preserve direct project-local .aider* files; Aider's indexed source_path is the chat history file and workspace is its parent"
    } else {
        "preserve project-local .aider* files when the planner receives the source path"
    };

    let mut notes = vec![
        "Aider migration support remains explicitly unsupported here; this is only a project-local state preservation hint."
            .to_string(),
        "references/coding_agent_session_search/tests/connector_aider.rs shows .aider.chat.history.md as source_path and the parent directory as workspace."
            .to_string(),
        "Aider is mostly a filesystem move problem; no global session rewrite adapter is expected for v1."
            .to_string(),
    ];
    if source_path_available && project_files.is_empty() {
        notes.push("no direct project-local .aider* files detected under source path".to_string());
    } else if !source_path_available {
        notes.push(
            "minimal contract upgrade: call aider::plan_for_source(old_path) from the migration planner to enumerate concrete .aider* files"
                .to_string(),
        );
    }

    HarnessMigrationReport::from_edits(
        AgentKind::Aider,
        AdapterReadiness::Unsupported,
        project_files.clone(),
        usize::from(project_files.iter().any(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(AIDER_CHAT_HISTORY)
        })),
        0,
        vec![MigrationEdit::preserve_project_local_history(
            AgentKind::Aider,
            target,
            detail,
        )],
        notes,
    )
}

fn discover_project_local_files(old_path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(old_path) else {
        return Vec::new();
    };

    let mut files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".aider"))
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_ops::MigrationEditKind;

    #[test]
    fn aider_no_source_keeps_migration_unsupported_explicit() {
        let report = plan();

        assert!(matches!(report.readiness, AdapterReadiness::Unsupported));
        assert!(report.state_roots.is_empty());
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("migration support remains explicitly unsupported")));
        assert!(report
            .operations
            .iter()
            .any(|op| op.target.contains("source path unavailable")));
    }

    #[test]
    fn aider_source_plan_reports_project_local_aider_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join(AIDER_CHAT_HISTORY), "> hello\n\nworld\n").unwrap();
        fs::write(root.join(".aider.input.history"), "/add src/main.rs\n").unwrap();
        fs::write(root.join("notes.md"), "not aider state\n").unwrap();

        let report = plan_for_source(root);

        assert!(matches!(report.readiness, AdapterReadiness::Unsupported));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 0);
        assert_eq!(report.state_roots.len(), 2);
        assert!(report
            .state_roots
            .iter()
            .any(|path| path.ends_with(AIDER_CHAT_HISTORY)));
        assert!(report.edits.iter().any(|edit| {
            matches!(
                &edit.kind,
                MigrationEditKind::PreserveProjectLocalHistory { target, detail }
                    if target.contains(AIDER_CHAT_HISTORY)
                        && target.contains(".aider.input.history")
                        && detail.contains("source_path")
            )
        }));
    }
}
