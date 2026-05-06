use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Result;

use crate::native_sessions::shared::{is_slash_command, NativeSession};
use crate::native_sessions::NativeSessionScanner;
use crate::utility::claude_storage::{claude_base, path_to_encoded};
use crate::AgentKind;

pub struct ClaudeScanner;

impl NativeSessionScanner for ClaudeScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan()
    }
}

/// Scan Claude Code's ~/.claude/history.jsonl.
fn scan() -> Result<Vec<NativeSession>> {
    let history_path = claude_base().join("history.jsonl");
    if !history_path.exists() {
        return Ok(Vec::new());
    }

    #[derive(serde::Deserialize)]
    struct Entry {
        display: String,
        #[serde(rename = "sessionId", default)]
        session_id: Option<String>,
        project: std::path::PathBuf,
        timestamp: i64,
    }

    struct Acc {
        project: String,
        first_real: Option<String>,
        last_real: Option<String>,
        first_ts: i64,
        last_ts: i64,
        turns: u32,
        all_commands: bool,
    }

    let file = std::fs::File::open(&history_path)?;
    let mut sessions: HashMap<String, Acc> = HashMap::new();

    for line in BufReader::new(file).lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let e: Entry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let sid = match e.session_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };
        let ts = e.timestamp / 1000;
        let project = e.project.to_string_lossy().into_owned();

        let is_cmd = is_slash_command(&e.display);
        let real = if is_cmd {
            None
        } else {
            Some(e.display.clone())
        };
        sessions
            .entry(sid)
            .and_modify(|acc| {
                if ts < acc.first_ts {
                    acc.first_ts = ts;
                    if !is_cmd && acc.first_real.is_none() {
                        acc.first_real = real.clone();
                    }
                }
                if ts >= acc.last_ts {
                    acc.last_ts = ts;
                    acc.project = project.clone();
                }
                if !is_cmd {
                    acc.all_commands = false;
                    acc.last_real = real.clone();
                    if acc.first_real.is_none() {
                        acc.first_real = real.clone();
                    }
                }
                acc.turns += 1;
            })
            .or_insert(Acc {
                project,
                first_real: real.clone(),
                last_real: real,
                first_ts: ts,
                last_ts: ts,
                turns: 1,
                all_commands: is_cmd,
            });
    }

    Ok(sessions
        .into_iter()
        .map(|(sid, acc)| {
            let custom = read_custom_title(&acc.project, &sid);
            let has_title = custom.is_some();
            let display_name = custom.or(acc.first_real);
            let last_prompt = if acc.turns > 1 { acc.last_real } else { None };

            NativeSession {
                agent_kind: AgentKind::Claude,
                native_id: sid,
                project_path: Some(acc.project),
                display_name,
                last_prompt,
                turn_count: acc.turns,
                last_seen_at: acc.last_ts,
                interactive: true,
                command_only: acc.all_commands,
                has_title,
            }
        })
        .collect())
}

fn read_custom_title(project_path: &str, session_id: &str) -> Option<String> {
    let encoded = path_to_encoded(Path::new(project_path));
    let session_path = claude_base()
        .join("projects")
        .join(encoded)
        .join(format!("{}.jsonl", session_id));

    let file = std::fs::File::open(&session_path).ok()?;
    let reader = BufReader::new(file);

    let mut title = None;
    for line in reader.lines().flatten() {
        if line.len() > 300 {
            continue;
        }
        if line.contains("\"custom-title\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("custom-title") {
                    if let Some(t) = v.get("customTitle").and_then(|v| v.as_str()) {
                        title = Some(t.to_string());
                    }
                }
            }
        }
    }
    title
}
