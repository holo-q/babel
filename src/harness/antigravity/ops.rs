use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{
    scan_text_refs, AdapterReadiness, HarnessMigrationReport, HarnessOpsContext, MigrationEdit,
};

#[derive(Default)]
struct AntigravityDiscovery {
    roots: Vec<PathBuf>,
    conversation_payloads: usize,
    brain_dirs: usize,
    brain_text_refs: usize,
    legacy_chat_files: usize,
    legacy_text_refs: usize,
    state_dbs: usize,
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

    if discovery.brain_text_refs > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Antigravity,
            "rewrite_brain_artifact_refs",
            "~/.gemini/antigravity/brain Markdown artifacts",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.brain_text_refs,
        ));
    }
    if discovery.legacy_text_refs > 0 {
        edits.push(MigrationEdit::rewrite_text_refs(
            AgentKind::Antigravity,
            "rewrite_legacy_code_tracker_refs",
            "~/.gemini/antigravity/code_tracker JSON/JSONL",
            old_path.display().to_string(),
            new_path.display().to_string(),
            discovery.legacy_text_refs,
        ));
    }
    if discovery.conversation_payloads > 0 || discovery.brain_dirs > 0 {
        edits.push(MigrationEdit::preserve_session_keyed_files(
            AgentKind::Antigravity,
            "preserve_conversation_payloads_and_brain_artifacts",
            antigravity_root(context),
            discovery.conversation_payloads + discovery.brain_dirs,
            discovery.brain_text_refs + discovery.legacy_text_refs,
        ));
    }

    let mut notes = vec![
        "storage candidates: ~/.gemini/antigravity/{conversations,brain,code_tracker} and platform state.vscdb paths".to_string(),
        "conversation payloads are protobuf .pb plus brain artifacts".to_string(),
    ];
    if discovery.roots.is_empty() {
        notes.push("no known Antigravity state roots detected".to_string());
    }
    if discovery.state_dbs > 0 {
        notes.push(format!(
            "detected {} Antigravity state DB(s); no typed SQLite edit is emitted without a schema-level migration contract",
            discovery.state_dbs
        ));
    }
    if discovery.truncated {
        notes.push(
            "one or more Antigravity text scans hit the shared file cap; narrow before apply"
                .to_string(),
        );
    }
    if discovery.large_files_sampled > 0 {
        notes.push(format!(
            "sampled {} large Antigravity text file(s) instead of full-reading them",
            discovery.large_files_sampled
        ));
    }
    if discovery.legacy_chat_files > 0 {
        notes.push(format!(
            "detected {} legacy code_tracker chat-shaped JSON/JSONL file(s)",
            discovery.legacy_chat_files
        ));
    }

    let sessions_found =
        discovery.conversation_payloads + discovery.brain_dirs + discovery.legacy_chat_files;
    let path_references_found = discovery.brain_text_refs + discovery.legacy_text_refs;
    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Antigravity,
        AdapterReadiness::DoctorOnly,
        discovery.roots,
        sessions_found,
        path_references_found,
        edits,
        notes,
    ))
}

fn discover(context: &HarnessOpsContext, needles: &[String]) -> Result<AntigravityDiscovery> {
    let mut discovery = AntigravityDiscovery::default();
    let root = antigravity_root(context);
    if root.exists() {
        discovery.roots.push(root.clone());
    }

    let conversations = root.join("conversations");
    if conversations.exists() {
        discovery.conversation_payloads = count_files_with_extension(&conversations, "pb")?;
        discovery.roots.push(conversations);
    }

    let brain = root.join("brain");
    if brain.exists() {
        discovery.brain_dirs = count_child_dirs(&brain)?;
        let scan = scan_text_refs(&brain, needles)?;
        discovery.brain_text_refs = scan.path_references_found;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        discovery.roots.push(brain);
    }

    let code_tracker = root.join("code_tracker");
    if code_tracker.exists() {
        discovery.legacy_chat_files = count_legacy_chat_files(&code_tracker)?;
        let scan = scan_text_refs(&code_tracker, needles)?;
        discovery.legacy_text_refs = scan.path_references_found;
        discovery.truncated |= scan.truncated;
        discovery.large_files_sampled += scan.large_files_sampled;
        discovery.roots.push(code_tracker);
    }

    for db in state_db_paths(context) {
        if db.exists() {
            discovery.state_dbs += 1;
            discovery.roots.push(db);
        }
    }

    discovery.roots.sort();
    discovery.roots.dedup();
    Ok(discovery)
}

fn antigravity_root(context: &HarnessOpsContext) -> PathBuf {
    context.home.join(".gemini/antigravity")
}

fn state_db_paths(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context
            .home
            .join(".config/Antigravity/User/globalStorage/state.vscdb"),
        context
            .home
            .join("Library/Application Support/Antigravity/User/globalStorage/state.vscdb"),
        context
            .home
            .join("AppData/Roaming/Antigravity/User/globalStorage/state.vscdb"),
    ]
}

fn count_child_dirs(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        if entry?.path().is_dir() {
            count += 1;
        }
    }
    Ok(count)
}

fn count_files_with_extension(root: &Path, extension: &str) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            count += count_files_with_extension(&path, extension)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
            count += 1;
        }
    }
    Ok(count)
}

fn count_legacy_chat_files(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            count += count_legacy_chat_files(&path)?;
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("json") | Some("jsonl")
        ) && legacy_file_has_chat_entries(&path)?
        {
            count += 1;
        }
    }
    Ok(count)
}

fn legacy_file_has_chat_entries(path: &Path) -> Result<bool> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(200) {
        let line = line?;
        let Some(json_start) = line.find('{') else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line[json_start..]) else {
            continue;
        };
        let role = value.get("type").and_then(|value| value.as_str());
        let content = value.get("content").and_then(|value| value.as_str());
        if matches!(role, Some("user") | Some("assistant"))
            && content.is_some_and(|s| !s.is_empty())
        {
            return Ok(true);
        }
    }
    Ok(false)
}
