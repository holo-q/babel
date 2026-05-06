use anyhow::{Context, Result};

use crate::native_sessions::shared::NativeSession;
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct GeminiScanner;

impl NativeSessionScanner for GeminiScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

/// Scan Gemini CLI's ~/.gemini/tmp/<project>/chats/.
fn scan() -> Result<Vec<NativeSession>> {
    let tmp_dir = dirs::home_dir()
        .context("no home dir")?
        .join(".gemini")
        .join("tmp");
    if !tmp_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for proj in std::fs::read_dir(&tmp_dir).into_iter().flatten().flatten() {
        let chats = proj.path().join("chats");
        if !chats.is_dir() {
            continue;
        }
        for chat in std::fs::read_dir(&chats).into_iter().flatten().flatten() {
            let p = chat.path();
            if !matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("json" | "jsonl")
            ) {
                continue;
            }
            let sid = p
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if sid.is_empty() {
                continue;
            }
            let mtime = chat
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(NativeSession {
                agent_kind: AgentKind::Gemini,
                native_id: sid,
                project_path: None,
                display_name: None,
                last_prompt: None,
                turn_count: 0,
                last_seen_at: mtime,
                interactive: true,
                command_only: false,
                has_title: false,
            });
        }
    }
    Ok(out)
}
