use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    scan_text_refs, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit,
    MAX_SCAN_FILES,
};

#[derive(Default)]
struct AmpDiscovery {
    roots: Vec<PathBuf>,
    thread_roots: Vec<PathBuf>,
    thread_root_scans: Vec<AmpThreadRootScan>,
    history_files: Vec<PathBuf>,
    config_files: Vec<PathBuf>,
    project_local_files: Vec<PathBuf>,
    thread_files: usize,
    matching_thread_files: usize,
    thread_path_ref_files: usize,
    history_path_ref_files: usize,
    config_path_ref_files: usize,
    project_local_path_ref_files: usize,
    files_scanned: usize,
    truncated: bool,
    large_files_sampled: usize,
}

#[derive(Default)]
struct AmpThreadRootScan {
    root: PathBuf,
    thread_files: usize,
    matching_thread_files: usize,
    path_ref_files: usize,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    let discovery = discover(context, old_path, needles)?;
    let mut edits = Vec::new();

    for scan in &discovery.thread_root_scans {
        if scan.path_ref_files > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Amp,
                    "rewrite_thread_path_refs",
                    scan.root.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_ref_files,
                )
                .with_apply_ready(true),
            );
        }
        if scan.thread_files > 0 {
            edits.push(MigrationEdit::preserve_session_keyed_files(
                AgentKind::Amp,
                "preserve_amp_thread_files",
                scan.root.clone(),
                scan.matching_thread_files,
                scan.path_ref_files,
            ));
        }
    }

    for history in &discovery.history_files {
        let scan = scan_text_refs(history, needles)?;
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Amp,
                    "rewrite_history_cwd_refs",
                    history.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true),
            );
        }
    }

    for config in discovery
        .config_files
        .iter()
        .chain(discovery.project_local_files.iter())
    {
        let scan = scan_text_refs(config, needles)?;
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Amp,
                    "rewrite_amp_config_refs",
                    config.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true),
            );
        }
    }

    if !discovery.project_local_files.is_empty() {
        edits.push(MigrationEdit::preserve_project_local_history(
            AgentKind::Amp,
            old_path.join(".amp").display().to_string(),
            "project-local .amp settings should move with the project directory; user-wide roots remain separate",
        ));
    }

    let path_references_found = discovery.thread_path_ref_files
        + discovery.history_path_ref_files
        + discovery.config_path_ref_files
        + discovery.project_local_path_ref_files;

    let mut notes = vec![
        "storage candidates: ~/.local/share/amp, ~/.config/amp, AMP_HOME, and AMP_DATA_HOME"
            .to_string(),
        "thread ids are provider-owned T-* identities".to_string(),
    ];

    if discovery.roots.is_empty() {
        notes.push("no known Amp state roots detected".to_string());
    }
    notes.push(format!(
        "discovered {} Amp thread file(s); {} contain source path references",
        discovery.thread_files, discovery.matching_thread_files
    ));
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} files; use a narrower Amp pass before applying",
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
        AgentKind::Amp,
        AdapterReadiness::ApplyReady,
        discovery.roots,
        discovery.matching_thread_files,
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(
    context: &HarnessOpsContext,
    old_path: &Path,
    needles: &[String],
) -> Result<AmpDiscovery> {
    let mut discovery = AmpDiscovery::default();
    let root_candidates = amp_roots(context);
    let thread_candidates = amp_thread_roots(context);
    let history_candidates = amp_history_files(context);
    let config_candidates = amp_config_files(context);
    let project_local_candidates = amp_project_local_files(old_path);

    discovery.roots = root_candidates
        .into_iter()
        .filter(|path| path.exists())
        .collect();
    discovery.thread_roots = thread_candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    discovery.history_files = history_candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    discovery.config_files = config_candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    discovery.project_local_files = project_local_candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect();

    let thread_roots = discovery.thread_roots.clone();
    for root in &thread_roots {
        let root_scan = scan_thread_root(root, needles, &mut discovery)?;
        discovery.thread_root_scans.push(root_scan);
    }

    for path in &discovery.history_files {
        let scan = scan_text_refs(path, needles)?;
        discovery.history_path_ref_files += scan.path_references_found;
        discovery.files_scanned += scan.files_scanned;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
    }

    for path in &discovery.config_files {
        let scan = scan_text_refs(path, needles)?;
        discovery.config_path_ref_files += scan.path_references_found;
        discovery.files_scanned += scan.files_scanned;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
    }

    for path in &discovery.project_local_files {
        let scan = scan_text_refs(path, needles)?;
        discovery.project_local_path_ref_files += scan.path_references_found;
        discovery.files_scanned += scan.files_scanned;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
    }

    discovery
        .roots
        .extend(discovery.thread_roots.iter().cloned());
    discovery
        .roots
        .extend(discovery.history_files.iter().cloned());
    discovery
        .roots
        .extend(discovery.config_files.iter().cloned());
    discovery
        .roots
        .extend(discovery.project_local_files.iter().cloned());
    discovery.roots.sort();
    discovery.roots.dedup();

    Ok(discovery)
}

fn scan_thread_root(
    root: &Path,
    needles: &[String],
    discovery: &mut AmpDiscovery,
) -> Result<AmpThreadRootScan> {
    let mut root_scan = AmpThreadRootScan {
        root: root.to_path_buf(),
        ..Default::default()
    };
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
        if !metadata.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        discovery.thread_files += 1;
        root_scan.thread_files += 1;
        let scan = scan_text_refs(&path, needles)?;
        discovery.files_scanned += scan.files_scanned;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        if scan.path_references_found > 0 {
            discovery.matching_thread_files += 1;
            discovery.thread_path_ref_files += scan.path_references_found;
            root_scan.matching_thread_files += 1;
            root_scan.path_ref_files += scan.path_references_found;
        }
    }

    Ok(root_scan)
}

fn amp_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut roots = vec![
        context.home.join(".config/amp"),
        context.home.join(".local/share/amp"),
        context.home.join(".amp/oauth"),
    ];
    roots.extend(legacy_extension_hosts(context).into_iter().map(|host| {
        host.join("User")
            .join("globalStorage")
            .join("sourcegraph.amp")
    }));
    roots.sort();
    roots.dedup();
    roots
}

fn amp_thread_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut roots = vec![
        context.home.join(".config/amp/threads"),
        context.home.join(".local/share/amp/threads"),
    ];
    roots.extend(legacy_extension_hosts(context).into_iter().map(|host| {
        host.join("User")
            .join("globalStorage")
            .join("sourcegraph.amp")
            .join("threads3")
    }));
    roots.sort();
    roots.dedup();
    roots
}

fn amp_history_files(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(".config/amp/history.jsonl"),
        context.home.join(".local/share/amp/history.jsonl"),
    ]
}

fn amp_config_files(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(".config/amp/settings.json"),
        context.home.join(".config/amp/settings.jsonc"),
        context.home.join(".config/amp/session.json"),
        context.home.join(".local/share/amp/session.json"),
        context.home.join(".local/share/amp/device-id.json"),
    ]
}

fn amp_project_local_files(project: &Path) -> Vec<PathBuf> {
    vec![
        project.join(".amp/settings.json"),
        project.join(".amp/settings.jsonc"),
    ]
}

fn legacy_extension_hosts(context: &HarnessOpsContext) -> Vec<PathBuf> {
    let mut hosts = Vec::new();
    for base in [
        context.home.join(".config"),
        context.home.join(".local/share"),
    ] {
        for name in ["Code", "Code - Insiders", "VSCodium", "Cursor", "Windsurf"] {
            hosts.push(base.join(name));
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn amp_doctor_reports_thread_history_and_project_settings_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let needles = vec![old.display().to_string()];

        write_file(
            &ctx.home.join(".config/amp/threads/T-abc.json"),
            &format!(
                r#"{{"id":"T-abc","env":{{"initial":{{"cwd":"{}"}}}},"messages":[]}}"#,
                old.display()
            ),
        );
        write_file(
            &ctx.home.join(".config/amp/history.jsonl"),
            &format!(r#"{{"text":"x","cwd":"{}"}}"#, old.display()),
        );
        write_file(
            &old.join(".amp/settings.json"),
            &format!(r#"{{"project":"{}"}}"#, old.display()),
        );

        let report = plan(&ctx, &old, &new, &needles).unwrap();

        assert_eq!(report.harness, AgentKind::Amp);
        assert!(matches!(report.readiness, AdapterReadiness::ApplyReady));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 3);
        assert!(report.edits.iter().any(|edit| edit.apply_ready));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "rewrite_thread_path_refs"));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "rewrite_history_cwd_refs"));
        assert!(report
            .operations
            .iter()
            .any(|op| op.action == "preserve_project_local_history"));
    }
}
