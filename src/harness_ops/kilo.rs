//! Kilo Code migration doctor adapter.
//!
//! Kilo is currently in Babel's unsupported lifecycle tier: no hooks, no
//! stable live identity surface, and no apply contract. The storage still
//! matters during project moves, so this adapter keeps the migration doctor
//! honest by reporting concrete extension roots and path-reference counts
//! without pretending mutation is safe.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    scan_text_refs, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit,
};

const KILO_EXTENSION_ID: &str = "kilocode.kilo-code";
const VSCODE_HOSTS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "Cursor",
    "Windsurf",
    "VSCodium",
    "Antigravity",
    "Kiro",
    "Trae",
];

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let candidates = kilo_storage_roots(context);
    let mut existing_roots = Vec::new();
    let mut sessions_found = 0;
    let mut path_references_found = 0;
    let mut notes = vec![
        "Kilo Code has no Babel lifecycle hooks today; live pane state cannot be migrated or verified from provider events."
            .to_string(),
        "References identify Kilo storage as VS Code-family extension JSON under User/globalStorage/kilocode.kilo-code/tasks/<taskId>/ui_messages.json."
            .to_string(),
        "Doctor preserves extension storage and reports path refs; apply stays disabled until Kilo workspace semantics and shutdown/backup rules are proven."
            .to_string(),
    ];

    for root in candidates {
        if !root.exists() {
            continue;
        }

        let tasks_root = root.join("tasks");
        let task_count = count_kilo_tasks(&tasks_root)?;
        let scan = scan_text_refs(&root, needles)?;

        sessions_found += task_count;
        path_references_found += scan.path_references_found;
        existing_roots.push(root.clone());

        notes.push(format!(
            "{}: {} task(s), {} path-ref file(s)",
            root.display(),
            task_count,
            scan.path_references_found
        ));
        if scan.truncated {
            notes.push(format!(
                "{}: scan stopped after {} files",
                root.display(),
                scan.files_scanned
            ));
        }
        if scan.large_files_sampled > 0 {
            notes.push(format!(
                "{}: sampled {} large file(s)",
                root.display(),
                scan.large_files_sampled
            ));
        }
    }

    if existing_roots.is_empty() {
        notes.push(format!(
            "no Kilo extension roots detected; probed {} VS Code-family host(s)",
            VSCODE_HOSTS.len()
        ));
    }

    let mut edits = Vec::new();
    for root in &existing_roots {
        let task_count = count_kilo_tasks(&root.join("tasks"))?;
        if task_count > 0 {
            edits.push(MigrationEdit::preserve_session_keyed_files(
                AgentKind::KiloCode,
                "preserve_kilo_tasks",
                root.join("tasks"),
                task_count,
                path_references_found,
            ));
        }
    }

    if path_references_found > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::KiloCode,
            "rewrite_kilo_path_refs_after_manual_backup",
            "VS Code globalStorage/kilocode.kilo-code JSON task/state files",
            old_path.display().to_string(),
            new_path.display().to_string(),
            path_references_found,
        ));
    }

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::KiloCode,
        AdapterReadiness::DoctorOnly,
        existing_roots,
        sessions_found,
        path_references_found,
        edits,
        notes,
    ))
}

fn kilo_storage_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for host in VSCODE_HOSTS {
        roots.push(
            context
                .home
                .join(".config")
                .join(host)
                .join("User/globalStorage")
                .join(KILO_EXTENSION_ID),
        );
        roots.push(
            context
                .home
                .join("Library/Application Support")
                .join(host)
                .join("User/globalStorage")
                .join(KILO_EXTENSION_ID),
        );
        roots.push(
            context
                .home
                .join("AppData/Roaming")
                .join(host)
                .join("User/globalStorage")
                .join(KILO_EXTENSION_ID),
        );
    }

    roots.sort();
    roots.dedup();
    roots
}

fn count_kilo_tasks(tasks_root: &Path) -> Result<usize> {
    if !tasks_root.exists() {
        return Ok(0);
    }

    let mut count = 0;
    for entry in std::fs::read_dir(tasks_root)? {
        let entry = entry?;
        let task_dir = entry.path();
        if entry.file_type()?.is_dir() && task_dir.join("ui_messages.json").exists() {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn kilo_reports_tasks_and_path_refs_without_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let root = home
            .join(".config/Code/User/globalStorage")
            .join(KILO_EXTENSION_ID);
        write_file(
            &root.join("tasks/task-a/ui_messages.json"),
            &format!("[{{\"type\":\"say\",\"text\":\"cwd {}\"}}]", old.display()),
        );
        write_file(
            &root.join("state/taskHistory.json"),
            &format!("{{\"task-a\":{{\"cwd\":\"{}\"}}}}", old.display()),
        );

        let needles = vec![old.display().to_string()];
        let report = plan(&ctx, &old, &new, &needles).unwrap();

        assert_eq!(report.harness, AgentKind::KiloCode);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 2);
        assert_eq!(report.state_roots, vec![root]);
        assert!(report.operations.iter().all(|op| !op.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "preserve_kilo_tasks"));
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("no Babel lifecycle hooks")));
    }

    #[test]
    fn kilo_missing_roots_stays_doctor_only_with_zero_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        let ctx = HarnessOpsContext::from_home(home.to_path_buf());

        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();

        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert!(report.state_roots.is_empty());
        assert_eq!(report.sessions_found, 0);
        assert_eq!(report.path_references_found, 0);
        assert!(report.operations.is_empty());
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("no Kilo extension roots detected")));
    }
}
