use anyhow::Result;

use crate::native_sessions::shared::{
    file_stem_id, json_array_facts, jsonl_facts, modified_secs, push_existing_path,
    query_sqlite_sessions, session_from_facts, visit_files, xdg_data_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct OpenCodeScanner;

impl NativeSessionScanner for OpenCodeScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

fn scan() -> Result<Vec<NativeSession>> {
    let mut roots = Vec::new();
    push_existing_path(&mut roots, xdg_data_path(&["opencode"]));
    let mut out = Vec::new();
    for root in &roots {
        let db = root.join("opencode.db");
        if db.is_file() {
            out.extend(query_sqlite_sessions(
                AgentKind::OpenCode,
                &db,
                &["session", "sessions"],
            ));
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    let mut files = Vec::new();
    for root in roots {
        visit_files(
            &root.join("storage"),
            5,
            &|path| {
                matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("json" | "jsonl")
                )
            },
            &mut files,
        );
    }
    Ok(files
        .into_iter()
        .filter_map(|path| {
            let facts = if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                jsonl_facts(&path)
            } else {
                json_array_facts(&path)
            };
            session_from_facts(
                AgentKind::OpenCode,
                file_stem_id(&path),
                None,
                None,
                modified_secs(&path),
                facts,
            )
        })
        .collect())
}
