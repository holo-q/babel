use anyhow::Result;

use crate::native_sessions::shared::{
    clean_session_text, env_path, home_path, json_epoch_secs, modified_secs, push_existing_path,
    NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct GithubCopilotScanner;

impl NativeSessionScanner for GithubCopilotScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::GithubCopilot
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

fn scan() -> Result<Vec<NativeSession>> {
    let mut roots = Vec::new();
    push_existing_path(
        &mut roots,
        env_path("COPILOT_HOME").map(|path| path.join("session-state")),
    );
    push_existing_path(&mut roots, home_path(&[".copilot", "session-state"]));

    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let workspace = dir.join("workspace.yaml");
            if !workspace.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&workspace).unwrap_or_default();
            let mut id = dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            let mut cwd = None;
            let mut title = None;
            let mut created_at = modified_secs(&workspace);
            let mut last_seen = created_at;
            for line in text.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim().trim_matches('"').trim_matches('\'');
                match key.trim() {
                    "id" if !value.is_empty() => id = Some(value.to_string()),
                    "cwd" | "workspace" | "workspace_path" if !value.is_empty() => {
                        cwd = Some(value.to_string())
                    }
                    "summary" | "title" if !value.is_empty() => title = clean_session_text(value),
                    "created_at" => {
                        if let Some(ts) =
                            json_epoch_secs(&serde_json::Value::String(value.to_string()))
                        {
                            created_at = ts;
                            last_seen = last_seen.max(ts);
                        }
                    }
                    "updated_at" => {
                        if let Some(ts) =
                            json_epoch_secs(&serde_json::Value::String(value.to_string()))
                        {
                            last_seen = last_seen.max(ts);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(native_id) = id {
                out.push(NativeSession {
                    agent_kind: AgentKind::GithubCopilot,
                    native_id,
                    project_path: cwd,
                    display_name: title,
                    last_prompt: None,
                    turn_count: 0,
                    created_at,
                    last_seen_at: last_seen,
                    interactive: true,
                    command_only: false,
                    has_title: true,
                });
            }
        }
    }
    Ok(out)
}
