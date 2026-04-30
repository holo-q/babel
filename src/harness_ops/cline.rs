use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    text_file_contains_any, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext,
    MigrationEdit, MAX_SCAN_FILES,
};

const CLINE_EXTENSION_ID: &str = "saoudrizwan.claude-dev";
const FILE_API_HISTORY: &str = "api_conversation_history.json";
const FILE_UI_MESSAGES: &str = "ui_messages.json";
const FILE_UI_MESSAGES_OLD: &str = "claude_messages.json";
const FILE_TASK_METADATA: &str = "task_metadata.json";
const FILE_TASK_HISTORY: &str = "taskHistory.json";
const FILE_HISTORY_ITEM: &str = "history_item.json";

#[derive(Default)]
struct ClineDiscovery {
    storage_roots: Vec<PathBuf>,
    workspace_storage_roots: Vec<PathBuf>,
    task_history_files: Vec<PathBuf>,
    tasks_roots: Vec<PathBuf>,
    matched_task_dirs: Vec<PathBuf>,
    task_history_ref_files: usize,
    task_metadata_ref_files: usize,
    transcript_ref_files: usize,
    other_task_ref_files: usize,
    task_dirs_scanned: usize,
    files_scanned: usize,
    truncated: bool,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, needles)?;
    let mut state_roots = Vec::new();
    state_roots.extend(discovery.storage_roots.iter().cloned());
    state_roots.extend(discovery.workspace_storage_roots.iter().cloned());
    state_roots.extend(discovery.tasks_roots.iter().cloned());
    state_roots.extend(discovery.task_history_files.iter().cloned());
    state_roots.extend(discovery.matched_task_dirs.iter().take(25).cloned());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    for task_history in &discovery.task_history_files {
        if file_contains_any(task_history, needles)? {
            edits.push(MigrationEdit::rewrite_text_refs(
                AgentKind::Cline,
                "rewrite_task_history_workspace_refs",
                task_history.display().to_string(),
                old_path.display().to_string(),
                new_path.display().to_string(),
                1,
            ));
        }
    }

    if discovery.task_metadata_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Cline,
            "rewrite_task_metadata_workspace_refs",
            "Cline tasks/*/task_metadata.json",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.task_metadata_ref_files,
        ));
    }
    if discovery.transcript_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Cline,
            "rewrite_task_transcript_path_refs",
            "Cline tasks/*/{ui_messages.json,api_conversation_history.json,claude_messages.json}",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.transcript_ref_files,
        ));
    }
    if discovery.other_task_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Cline,
            "rewrite_task_companion_path_refs",
            "Cline tasks/*/history_item.json and companion task files",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.other_task_ref_files,
        ));
    }
    for tasks_root in &discovery.tasks_roots {
        let matched_under_root = discovery
            .matched_task_dirs
            .iter()
            .filter(|task_dir| task_dir.starts_with(tasks_root))
            .count();
        if matched_under_root > 0 {
            edits.push(MigrationEdit::preserve_session_keyed_files(
                AgentKind::Cline,
                "preserve_cline_task_folders",
                tasks_root.clone(),
                matched_under_root,
                discovery.transcript_ref_files + discovery.task_metadata_ref_files,
            ));
        }
    }

    let mut notes = vec![
        "storage: <host>/User/globalStorage/saoudrizwan.claude-dev/".to_string(),
        "task index: state/taskHistory.json; task folders hold ui_messages, api_conversation_history, and task_metadata".to_string(),
    ];
    if discovery.storage_roots.is_empty() {
        notes.push(format!(
            "no Cline extension globalStorage root detected; probed {} candidate(s)",
            cline_storage_candidates(context).len()
        ));
    }
    if discovery.workspace_storage_roots.is_empty() {
        notes.push(format!(
            "no VS Code workspaceStorage root detected for Cline host candidates; probed {} candidate(s)",
            workspace_storage_candidates(context).len()
        ));
    }
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} task dirs and {} files; use a narrower pass before apply",
            discovery.task_dirs_scanned, discovery.files_scanned
        ));
    }
    if !discovery.matched_task_dirs.is_empty() {
        notes.push(format!(
            "matched Cline task folder(s): {}",
            discovery
                .matched_task_dirs
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let path_references_found = discovery.task_history_ref_files
        + discovery.task_metadata_ref_files
        + discovery.transcript_ref_files
        + discovery.other_task_ref_files;

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Cline,
        AdapterReadiness::DoctorOnly,
        state_roots,
        discovery.matched_task_dirs.len(),
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    needles: &[String],
) -> Result<ClineDiscovery> {
    let mut discovery = ClineDiscovery::default();
    let child_prefix = format!("{}/", old_path.display());

    discovery.workspace_storage_roots = workspace_storage_candidates(context)
        .into_iter()
        .filter(|root| root.exists())
        .collect();

    for storage_root in cline_storage_candidates(context) {
        if !storage_root.exists() {
            continue;
        }

        discovery.storage_roots.push(storage_root.clone());
        let tasks_root = storage_root.join("tasks");
        if tasks_root.exists() {
            discovery.tasks_roots.push(tasks_root.clone());
            scan_tasks_root(
                &tasks_root,
                old_path,
                &child_prefix,
                needles,
                &mut discovery,
            )?;
        }

        for task_history in task_history_candidates(&storage_root) {
            if !task_history.exists() {
                continue;
            }
            if file_contains_any(&task_history, needles)? {
                discovery.task_history_ref_files += 1;
            }
            if let Some(task_ids) =
                task_history_workspace_task_ids(&task_history, old_path, &child_prefix)?
            {
                for task_id in task_ids {
                    let task_dir = tasks_root.join(task_id);
                    if task_dir.exists() && !discovery.matched_task_dirs.contains(&task_dir) {
                        discovery.matched_task_dirs.push(task_dir);
                    }
                }
            }
            discovery.task_history_files.push(task_history);
        }
    }

    discovery.storage_roots.sort();
    discovery.storage_roots.dedup();
    discovery.workspace_storage_roots.sort();
    discovery.workspace_storage_roots.dedup();
    discovery.task_history_files.sort();
    discovery.task_history_files.dedup();
    discovery.tasks_roots.sort();
    discovery.tasks_roots.dedup();
    discovery.matched_task_dirs.sort();
    discovery.matched_task_dirs.dedup();
    Ok(discovery)
}

fn scan_tasks_root(
    tasks_root: &Path,
    old_path: &Path,
    child_prefix: &str,
    needles: &[String],
    discovery: &mut ClineDiscovery,
) -> Result<()> {
    for entry in fs::read_dir(tasks_root)? {
        if discovery.task_dirs_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let entry = entry?;
        let task_dir = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&task_dir) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        discovery.task_dirs_scanned += 1;

        let mut task_matched = false;
        for file_name in [
            FILE_TASK_METADATA,
            FILE_UI_MESSAGES,
            FILE_API_HISTORY,
            FILE_UI_MESSAGES_OLD,
            FILE_HISTORY_ITEM,
        ] {
            if discovery.files_scanned >= MAX_SCAN_FILES {
                discovery.truncated = true;
                break;
            }

            let path = task_dir.join(file_name);
            if !path.exists() {
                continue;
            }
            discovery.files_scanned += 1;
            let contains_ref = file_contains_any(&path, needles)?;
            if contains_ref {
                match file_name {
                    FILE_TASK_METADATA => discovery.task_metadata_ref_files += 1,
                    FILE_UI_MESSAGES | FILE_API_HISTORY | FILE_UI_MESSAGES_OLD => {
                        discovery.transcript_ref_files += 1;
                    }
                    _ => discovery.other_task_ref_files += 1,
                }
            }
            if file_name == FILE_TASK_METADATA
                && metadata_workspace_matches(&path, old_path, child_prefix)?
            {
                task_matched = true;
            }
        }

        if task_matched {
            discovery.matched_task_dirs.push(task_dir);
        }
    }
    Ok(())
}

fn file_contains_any(path: &Path, needles: &[String]) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Ok(false);
    }
    text_file_contains_any(path, metadata.len(), needles)
}

fn metadata_workspace_matches(path: &Path, old_path: &Path, child_prefix: &str) -> Result<bool> {
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(false);
    };
    Ok(["rootPath", "cwd", "workspace", "cwdOnTaskInitialization"]
        .iter()
        .filter_map(|key| value.get(key).and_then(|value| value.as_str()))
        .any(|workspace| workspace_matches(workspace, old_path, child_prefix)))
}

fn task_history_workspace_task_ids(
    path: &Path,
    old_path: &Path,
    child_prefix: &str,
) -> Result<Option<Vec<String>>> {
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Ok(None);
    };

    let mut task_ids = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let matched = ["cwdOnTaskInitialization", "workspace", "rootPath", "cwd"]
            .iter()
            .filter_map(|key| obj.get(*key).and_then(|value| value.as_str()))
            .any(|workspace| workspace_matches(workspace, old_path, child_prefix));
        if !matched {
            continue;
        }
        if let Some(id) = obj.get("id").and_then(|value| value.as_str()) {
            task_ids.push(id.to_string());
        }
    }

    Ok(Some(task_ids))
}

fn workspace_matches(workspace: &str, old_path: &Path, child_prefix: &str) -> bool {
    let old = old_path.to_string_lossy();
    workspace == old.as_ref() || workspace.starts_with(child_prefix)
}

fn task_history_candidates(storage_root: &Path) -> Vec<PathBuf> {
    vec![
        storage_root.join("state").join(FILE_TASK_HISTORY),
        storage_root.join(FILE_TASK_HISTORY),
        storage_root.join("tasks").join("_index.json"),
        storage_root.join("_index.json"),
    ]
}

fn cline_storage_candidates(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    if let Ok(home) = std::env::var("CLINE_HOME") {
        candidates.insert(PathBuf::from(home));
    }
    if let Ok(storage) = std::env::var("CLINE_STORAGE_PATH") {
        candidates.insert(PathBuf::from(storage));
    }
    if let Ok(storage) = std::env::var("CONTINUES_CLINE_STORAGE_PATH") {
        candidates.insert(PathBuf::from(storage));
    }
    if let Ok(cline_dir) = std::env::var("CLINE_DIR") {
        candidates.insert(PathBuf::from(cline_dir).join("data"));
    }
    candidates.insert(context.home.join(".cline/data"));
    for base in global_storage_bases(context) {
        candidates.insert(base.join(CLINE_EXTENSION_ID));
    }
    candidates.into_iter().collect()
}

fn workspace_storage_candidates(context: &HarnessOpsContext) -> Vec<PathBuf> {
    host_config_roots(context)
        .into_iter()
        .map(|host| host.join("User/workspaceStorage"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn global_storage_bases(context: &HarnessOpsContext) -> Vec<PathBuf> {
    host_config_roots(context)
        .into_iter()
        .map(|host| host.join("User/globalStorage"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn host_config_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let home = &context.home;
    [
        home.join(".config/Code"),
        home.join(".config/Code - Insiders"),
        home.join(".config/VSCodium"),
        home.join(".config/Cursor"),
        home.join(".config/Windsurf"),
        home.join(".vscode-server/data"),
        home.join(".vscode-server-insiders/data"),
        home.join(".cursor-server/data"),
        home.join(".cursor-server-insiders/data"),
        home.join("Library/Application Support/Code"),
        home.join("Library/Application Support/Code - Insiders"),
        home.join("Library/Application Support/VSCodium"),
        home.join("Library/Application Support/Cursor"),
        home.join("Library/Application Support/Windsurf"),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn cline_reports_task_history_and_task_folder_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("repo");
        let new = home.join("repo-renamed");
        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let storage = home
            .join(".config/Code/User/globalStorage")
            .join(CLINE_EXTENSION_ID);
        let task_dir = storage.join("tasks/1700001234567");

        write_file(
            &storage.join("state/taskHistory.json"),
            &format!(
                "[{{\"id\":\"1700001234567\",\"task\":\"x\",\"cwdOnTaskInitialization\":\"{}\"}}]\n",
                old.display()
            ),
        );
        write_file(
            &task_dir.join("task_metadata.json"),
            &format!("{{\"rootPath\":\"{}\"}}\n", old.display()),
        );
        write_file(
            &task_dir.join("ui_messages.json"),
            &format!(
                "[{{\"type\":\"say\",\"say\":\"task\",\"text\":\"cd {}\"}}]\n",
                old.display()
            ),
        );
        fs::create_dir_all(home.join(".config/Code/User/workspaceStorage")).unwrap();

        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 3);
        assert!(report.operations.iter().all(|op| !op.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "rewrite_task_history_workspace_refs"));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "preserve_cline_task_folders"));
        assert!(report
            .state_roots
            .iter()
            .any(|root| root.ends_with("User/workspaceStorage")));
    }
}
