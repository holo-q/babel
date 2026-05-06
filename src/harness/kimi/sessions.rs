use anyhow::{Context, Result};

use crate::native_sessions::shared::{
    env_path, home_path, jsonl_facts, modified_secs, read_json, session_from_facts,
    update_facts_from_record, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct KimiScanner;

impl NativeSessionScanner for KimiScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Kimi
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

/// Scan Kimi CLI's ~/.kimi/sessions/ store.
fn scan() -> Result<Vec<NativeSession>> {
    let kimi_dir = env_path("KIMI_SHARE_DIR")
        .or_else(|| home_path(&[".kimi"]))
        .context("no Kimi share dir")?
        .join("sessions");
    if !kimi_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for workdir in std::fs::read_dir(&kimi_dir).into_iter().flatten().flatten() {
        if !workdir.path().is_dir() {
            continue;
        }
        for sess in std::fs::read_dir(workdir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = sess.path();
            let context_path = if path.is_dir() {
                path.join("context.jsonl")
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                path.clone()
            } else {
                continue;
            };
            if !context_path.is_file() {
                continue;
            }
            let sid = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if sid.is_empty() {
                continue;
            }
            let mut facts = jsonl_facts(&context_path);
            if path.is_dir() {
                for metadata_name in ["state.json", "metadata.json"] {
                    if let Some(metadata) = read_json(&path.join(metadata_name)) {
                        update_facts_from_record(&mut facts, &metadata);
                    }
                }
            }
            if let Some(session) = session_from_facts(
                AgentKind::Kimi,
                Some(sid),
                None,
                None,
                modified_secs(&context_path),
                facts,
            ) {
                out.push(session);
            }
        }
    }
    Ok(out)
}
