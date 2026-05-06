use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    is_probably_text_state_file, text_file_contains_any, AdapterReadiness, HarnessMigrationReport,
    HarnessOpsContext, MigrationEdit, MigrationRisk, RiskSeverity, MAX_SCAN_FILES,
};

#[derive(Default)]
struct CopilotDiscovery {
    sessions_found: usize,
    path_references_found: usize,
    files_scanned: usize,
    truncated: bool,
}

pub(super) fn plan(
    context: &HarnessOpsContext,
    needles: &[String],
    risks: &mut Vec<MigrationRisk>,
) -> Result<HarnessMigrationReport> {
    let roots = copilot_roots(context);
    let session_state = roots.session_state.clone();
    let discovery = discover(&session_state, needles)?;

    if session_state.exists() {
        risks.push(MigrationRisk {
            severity: if discovery.path_references_found > 0 {
                RiskSeverity::Blocker
            } else {
                RiskSeverity::Warning
            },
            harness: Some(AgentKind::GithubCopilot),
            message:
                "GitHub Copilot CLI does not expose a stable migration session/task id; Babel must not infer identity from cwd or timestamps"
                    .to_string(),
        });
    }

    let mut state_roots = vec![roots.base, session_state.clone()];
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    let mut edits = Vec::new();
    if session_state.exists() {
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::GithubCopilot,
            "preserve_copilot_session_state",
            session_state.clone(),
            discovery.sessions_found,
            discovery.path_references_found,
        ));
    }

    let mut notes = vec![
        "storage candidate: ~/.copilot/session-state/<session>/events.jsonl".to_string(),
        "no stable migration session id found in local CLI state".to_string(),
    ];

    if !session_state.exists() {
        notes.push(format!("state root missing: {}", session_state.display()));
    }
    if discovery.truncated {
        notes.push(format!(
            "scan stopped after {} files; path reference count is incomplete",
            discovery.files_scanned
        ));
    }

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::GithubCopilot,
        AdapterReadiness::Unsupported,
        state_roots,
        discovery.sessions_found,
        discovery.path_references_found,
        edits,
        notes,
    ))
}

struct CopilotRoots {
    base: PathBuf,
    session_state: PathBuf,
}

fn copilot_roots(context: &HarnessOpsContext) -> CopilotRoots {
    let system_home = dirs::home_dir();
    let base = if system_home.as_ref() == Some(&context.home) {
        std::env::var_os("COPILOT_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| context.home.join(".copilot"))
    } else {
        context.home.join(".copilot")
    };

    CopilotRoots {
        session_state: base.join("session-state"),
        base,
    }
}

fn discover(session_state: &Path, needles: &[String]) -> Result<CopilotDiscovery> {
    let mut discovery = CopilotDiscovery::default();
    if !session_state.exists() {
        return Ok(discovery);
    }

    let mut stack = vec![session_state.to_path_buf()];
    while let Some(path) = stack.pop() {
        if discovery.files_scanned >= MAX_SCAN_FILES {
            discovery.truncated = true;
            break;
        }

        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if path.join("events.jsonl").is_file() || path.join("workspace.yaml").is_file() {
                discovery.sessions_found += 1;
            }
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
            continue;
        }

        if !metadata.is_file() || !is_copilot_text_state_file(&path) {
            continue;
        }

        discovery.files_scanned += 1;
        let Ok(found) = text_file_contains_any(&path, metadata.len(), needles) else {
            continue;
        };
        if found {
            discovery.path_references_found += 1;
        }
    }

    Ok(discovery)
}

fn is_copilot_text_state_file(path: &Path) -> bool {
    is_probably_text_state_file(path)
        || matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yaml") | Some("yml")
        )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn discovers_chronicle_storage_without_apply_ready_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let session = home
            .join(".copilot")
            .join("session-state")
            .join("session-123");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("events.jsonl"),
            r#"{"type":"session.start","data":{"sessionId":"session-123","cwd":"/old/project"}}
{"type":"user.message","data":{"content":"open /old/project/src/lib.rs"}}"#,
        )
        .unwrap();

        let context = HarnessOpsContext::from_home(home);
        let mut risks = Vec::new();
        let report = plan(&context, &[String::from("/old/project")], &mut risks).unwrap();

        assert_eq!(report.harness, AgentKind::GithubCopilot);
        assert!(matches!(report.readiness, AdapterReadiness::Unsupported));
        assert_eq!(report.sessions_found, 1);
        assert_eq!(report.path_references_found, 1);
        assert!(report
            .operations
            .iter()
            .all(|operation| !operation.apply_ready));
        assert!(risks.iter().any(|risk| {
            matches!(risk.severity, RiskSeverity::Blocker)
                && risk.message.contains("must not infer identity")
        }));
    }
}
