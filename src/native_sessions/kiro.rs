use anyhow::Result;

use crate::native_sessions::shared::{
    file_stem_id, home_path, json_array_facts, jsonl_facts, modified_secs, push_existing_path,
    session_from_facts, visit_files, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct KiroScanner;

impl NativeSessionScanner for KiroScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Kiro
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

fn scan() -> Result<Vec<NativeSession>> {
    let mut roots = Vec::new();
    push_existing_path(
        &mut roots,
        home_path(&[
            "Library",
            "Application Support",
            "Kiro",
            "User",
            "globalStorage",
            "kiro.kiroagent",
            "workspace-sessions",
        ]),
    );
    push_existing_path(
        &mut roots,
        home_path(&[
            ".config",
            "Kiro",
            "User",
            "globalStorage",
            "kiro.kiroagent",
            "workspace-sessions",
        ]),
    );
    push_existing_path(&mut roots, home_path(&[".kiro", "sessions", "cli"]));
    let mut files = Vec::new();
    for root in roots {
        visit_files(
            &root,
            4,
            &|path| {
                matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("json" | "jsonl")
                )
            },
            &mut files,
        );
    }
    let mut out = Vec::new();
    for path in files {
        let facts = if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            jsonl_facts(&path)
        } else {
            json_array_facts(&path)
        };
        if let Some(session) = session_from_facts(
            AgentKind::Kiro,
            file_stem_id(&path),
            None,
            None,
            modified_secs(&path),
            facts,
        ) {
            out.push(session);
        }
    }
    Ok(out)
}
