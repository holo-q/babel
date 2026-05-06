use anyhow::Result;

use crate::native_sessions::shared::{
    clean_session_text, file_stem_id, json_str, json_time, modified_secs, push_existing_path,
    read_json, session_from_facts, update_facts_from_record, visit_files, HarnessTranscriptFacts,
    NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct AmpScanner;

impl NativeSessionScanner for AmpScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Amp
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

fn scan() -> Result<Vec<NativeSession>> {
    let mut roots = Vec::new();
    push_existing_path(
        &mut roots,
        crate::native_sessions::shared::xdg_data_path(&["amp", "threads"]),
    );
    let mut out = Vec::new();
    let mut files = Vec::new();
    for root in roots {
        visit_files(
            &root,
            2,
            &|path| path.extension().and_then(|e| e.to_str()) == Some("json"),
            &mut files,
        );
    }
    for path in files {
        let Some(value) = read_json(&path) else {
            continue;
        };
        let id = json_str(&value, &["id"])
            .map(str::to_string)
            .or_else(|| file_stem_id(&path));
        let title = json_str(&value, &["title"]).and_then(clean_session_text);
        let cwd = value
            .get("env")
            .and_then(|env| env.get("initial"))
            .and_then(|initial| initial.get("trees"))
            .and_then(|trees| trees.as_array())
            .and_then(|trees| trees.first())
            .and_then(|tree| tree.get("uri"))
            .and_then(|uri| uri.as_str())
            .map(|uri| uri.strip_prefix("file://").unwrap_or(uri).to_string());
        let mut facts = HarnessTranscriptFacts {
            native_id: id,
            project_path: cwd,
            display_name: title,
            ..Default::default()
        };
        if let Some(messages) = value
            .get("messages")
            .and_then(|messages| messages.as_array())
        {
            for message in messages {
                facts.line_count += 1;
                update_facts_from_record(&mut facts, message);
            }
        }
        if let Some(ts) = json_time(&value, &["created", "createdAt", "updatedAt"]) {
            facts.last_seen_at = ts;
        }
        if let Some(session) = session_from_facts(
            AgentKind::Amp,
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
