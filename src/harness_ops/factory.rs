use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    is_probably_text_state_file, text_file_contains_any, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, MigrationEdit, MAX_SCAN_BYTES, MAX_SCAN_FILES,
};

const FACTORY_PROJECTS: &str = ".factory/projects";
const FACTORY_SESSIONS: &str = ".factory/sessions";

#[derive(Default)]
struct FactoryDiscovery {
    matched_sessions: Vec<FactorySession>,
    session_path_ref_files: usize,
    settings_ref_files: usize,
    root_settings_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

struct FactorySession {
    id: String,
    path: PathBuf,
    root_kind: FactoryRootKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FactoryRootKind {
    Projects,
    Sessions,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let projects_root = context.home.join(FACTORY_PROJECTS);
    let sessions_root = context.home.join(FACTORY_SESSIONS);
    let root_settings = vec![
        context.home.join(".factory/settings.json"),
        context.home.join(".factory/settings.local.json"),
        old_path.join(".factory/settings.local.json"),
    ];
    let discovery = discover(
        &projects_root,
        &sessions_root,
        &root_settings,
        old_path,
        needles,
    )?;

    let mut state_roots = vec![projects_root.clone(), sessions_root.clone()];
    state_roots.extend(root_settings.iter().cloned());
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    if discovery.session_path_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::FactoryDroid,
            "rewrite_transcript_path_refs",
            "~/.factory/{projects,sessions}/<workspace>/<session>.jsonl",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.session_path_ref_files,
        ));
    }
    if discovery.settings_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::FactoryDroid,
            "rewrite_session_settings_refs",
            "~/.factory/{projects,sessions}/<workspace>/<session>.settings.json",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.settings_ref_files,
        ));
    }
    if discovery.root_settings_ref_files > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::FactoryDroid,
            "rewrite_factory_settings_refs",
            "~/.factory/settings*.json and <project>/.factory/settings.local.json",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.root_settings_ref_files,
        ));
    }

    let mut notes = vec![
        "storage candidates: ~/.factory/projects and ~/.factory/sessions".to_string(),
        "session records may have sibling .settings.json files".to_string(),
    ];

    for root in [&projects_root, &sessions_root] {
        if !root.exists() {
            notes.push(format!("state root missing: {}", root.display()));
        }
    }
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} files; use a narrower Factory pass before any apply",
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
            .take(4)
            .map(|session| {
                let root = match session.root_kind {
                    FactoryRootKind::Projects => "projects",
                    FactoryRootKind::Sessions => "sessions",
                };
                format!("{} [{}] ({})", session.id, root, session.path.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if discovery.matched_sessions.len() > 4 {
            ", ..."
        } else {
            ""
        };
        notes.push(format!(
            "matched Factory Droid session id(s): {ids}{suffix}"
        ));
    }

    let path_references_found = discovery.session_path_ref_files
        + discovery.settings_ref_files
        + discovery.root_settings_ref_files;

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::FactoryDroid,
        AdapterReadiness::DoctorOnly,
        state_roots,
        discovery.matched_sessions.len(),
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    projects_root: &Path,
    sessions_root: &Path,
    root_settings: &[PathBuf],
    old_path: &Path,
    needles: &[String],
) -> Result<FactoryDiscovery> {
    let mut discovery = FactoryDiscovery::default();
    let child_prefix = format!("{}/", old_path.display());

    collect_sessions_from_root(
        projects_root,
        FactoryRootKind::Projects,
        old_path,
        &child_prefix,
        needles,
        &mut discovery,
    )?;
    collect_sessions_from_root(
        sessions_root,
        FactoryRootKind::Sessions,
        old_path,
        &child_prefix,
        needles,
        &mut discovery,
    )?;

    let mut seen = BTreeSet::new();
    discovery.matched_sessions.retain(|session| {
        let key = session.id.clone();
        if seen.contains(&key) {
            return false;
        }
        seen.insert(key);
        true
    });

    for path in root_settings {
        discovery.root_settings_ref_files += text_file_ref_count(path, needles)?;
    }

    Ok(discovery)
}

fn collect_sessions_from_root(
    root: &Path,
    root_kind: FactoryRootKind,
    old_path: &Path,
    child_prefix: &str,
    needles: &[String],
    discovery: &mut FactoryDiscovery,
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
        if !metadata.is_file() {
            continue;
        }

        let is_jsonl = path.extension().and_then(|ext| ext.to_str()) == Some("jsonl");
        let is_settings = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".settings.json"));
        if !is_jsonl && !is_settings {
            continue;
        }

        discovery.files_scanned += 1;
        if text_file_contains_any(&path, metadata.len(), needles)? {
            if is_settings {
                discovery.settings_ref_files += 1;
            } else {
                discovery.session_path_ref_files += 1;
            }
            if metadata.len() > MAX_SCAN_BYTES {
                discovery.large_files_sampled += 1;
            }
        }

        if is_jsonl {
            if let Some(session) = read_session_identity(&path, root_kind, old_path, child_prefix)?
            {
                discovery.matched_sessions.push(session);
            }
        }
    }

    Ok(())
}

fn read_session_identity(
    path: &Path,
    root_kind: FactoryRootKind,
    old_path: &Path,
    child_prefix: &str,
) -> Result<Option<FactorySession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines().take(150) {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|value| value.as_str()) != Some("session_start") {
            continue;
        }
        let cwd = value.get("cwd").and_then(|value| value.as_str());
        if !cwd_matches(cwd, old_path, child_prefix) {
            return Ok(None);
        }
        let id = value
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| session_id_from_path(path));
        return Ok(Some(FactorySession {
            id,
            path: path.to_path_buf(),
            root_kind,
        }));
    }

    Ok(None)
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
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
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
    fn factory_reports_projects_and_sessions_without_apply_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let id = "11111111-1111-4111-8111-111111111111";
        let project_session = home
            .join(FACTORY_PROJECTS)
            .join("workspace-old")
            .join(format!("{id}.jsonl"));
        write_file(
            &project_session,
            &format!(
                "{{\"type\":\"session_start\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
                old.display(),
                old.display()
            ),
        );
        write_file(
            &home
                .join(FACTORY_PROJECTS)
                .join("workspace-old")
                .join(format!("{id}.settings.json")),
            &format!("{{\"lastCwd\":\"{}\"}}\n", old.display()),
        );

        let report = plan(&ctx, &old, &new, &[old.display().to_string()]).unwrap();

        assert_eq!(report.harness, AgentKind::FactoryDroid);
        assert!(matches!(report.readiness, AdapterReadiness::DoctorOnly));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 2);
        assert!(report.operations.iter().all(|op| !op.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "rewrite_transcript_path_refs"));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "rewrite_session_settings_refs"));
    }

    #[test]
    fn factory_dedupes_projects_before_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let id = "22222222-2222-4222-8222-222222222222";
        let row = format!(
            "{{\"type\":\"session_start\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n",
            old.display()
        );

        write_file(
            &home
                .join(FACTORY_PROJECTS)
                .join("workspace-old")
                .join(format!("{id}.jsonl")),
            &row,
        );
        write_file(
            &home
                .join(FACTORY_SESSIONS)
                .join("workspace-old")
                .join(format!("{id}.jsonl")),
            &row,
        );

        let report = plan(
            &ctx,
            &old,
            &home.join("Workspace/new"),
            &[old.display().to_string()],
        )
        .unwrap();
        assert_eq!(report.sessions_found, 1);
        assert!(report.notes.iter().any(|note| note.contains("[projects]")));
    }
}
