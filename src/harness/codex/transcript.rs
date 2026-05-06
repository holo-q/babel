//! Codex rollout transcript loading.
//!
//! Listing metadata comes from `state_*.sqlite`, but the readable transcript
//! lives in rollout JSONL. Keep that protocol here with the rest of Codex so the
//! pager only asks for normalized `scrollparse::Message` rows.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use scrollparse::{Message, MessageKind};
use serde::Deserialize;

pub fn find_session_transcript(session_id: &str) -> Result<Option<PathBuf>> {
    let codex_home = dirs::home_dir().context("no home dir")?.join(".codex");
    if !codex_home.exists() {
        return Ok(None);
    }

    if let Some(path) = find_transcript_from_state_db(&codex_home, session_id)? {
        if path.exists() {
            return Ok(Some(path));
        }
    }

    find_transcript_by_filename(&codex_home.join("sessions"), session_id)
}

pub fn parse_transcript(path: &Path) -> Result<Vec<Message>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut line_num = 0;

    for line in reader.lines() {
        line_num += 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let record: RolloutRecord = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(_) => continue,
        };

        match record {
            RolloutRecord::EventMsg {
                payload: EventPayload::UserMessage { message },
                ..
            } => push_message(&mut messages, MessageKind::User, message, line_num),
            RolloutRecord::ResponseItem {
                payload: ResponsePayload::Message { role, content },
                ..
            } if role == "assistant" => {
                let content = extract_content(&content);
                push_message(&mut messages, MessageKind::Assistant, content, line_num);
            }
            RolloutRecord::ResponseItem {
                payload:
                    ResponsePayload::FunctionCall {
                        name, arguments, ..
                    },
                ..
            } => {
                messages.push(Message {
                    kind: MessageKind::ToolCall {
                        name,
                        args: arguments.unwrap_or_default(),
                    },
                    content: String::new(),
                    line: line_num,
                });
            }
            RolloutRecord::ResponseItem {
                payload: ResponsePayload::FunctionCallOutput { output, .. },
                ..
            } => {
                let content = output_to_string(output);
                push_message(&mut messages, MessageKind::ToolOutput, content, line_num);
            }
            _ => {}
        }
    }

    Ok(messages)
}

fn find_transcript_from_state_db(codex_home: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let Some(db_path) = find_state_db(codex_home) else {
        return Ok(None);
    };
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let has_rollout_path: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads') WHERE name='rollout_path')",
        [],
        |row| row.get(0),
    )?;
    if !has_rollout_path {
        return Ok(None);
    }

    let mut stmt = conn.prepare("SELECT rollout_path FROM threads WHERE id = ?1 LIMIT 1")?;
    let mut rows = stmt.query([session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let path: String = row.get(0)?;
    Ok(Some(PathBuf::from(path)))
}

fn find_state_db(codex_home: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.starts_with("state_") && name.ends_with(".sqlite") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

fn find_transcript_by_filename(root: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    if !root.exists() {
        return Ok(None);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.ends_with(".jsonl") && name.contains(session_id) {
                return Ok(Some(path));
            }
        }
    }

    Ok(None)
}

fn push_message(messages: &mut Vec<Message>, kind: MessageKind, content: String, line_num: usize) {
    if content.trim().is_empty() {
        return;
    }
    messages.push(Message {
        kind,
        content,
        line: line_num,
    });
}

fn extract_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::InputText { text }
            | ContentBlock::OutputText { text }
            | ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_to_string(output: Option<serde_json::Value>) -> String {
    match output {
        Some(serde_json::Value::String(text)) => text,
        Some(value) => serde_json::to_string(&value).unwrap_or_default(),
        None => String::new(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RolloutRecord {
    ResponseItem {
        payload: ResponsePayload,
    },
    EventMsg {
        payload: EventPayload,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsePayload {
    Message {
        role: String,
        #[serde(default)]
        content: Vec<ContentBlock>,
    },
    FunctionCall {
        name: String,
        arguments: Option<String>,
        #[allow(dead_code)]
        call_id: Option<String>,
    },
    FunctionCallOutput {
        output: Option<serde_json::Value>,
        #[allow(dead_code)]
        call_id: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventPayload {
    UserMessage {
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    InputText {
        text: String,
    },
    OutputText {
        text: String,
    },
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn parse_codex_rollout_messages() {
        let path = std::env::current_dir()
            .unwrap()
            .join("tmp")
            .join(format!("codex-transcript-{}.jsonl", std::process::id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<environment_context>noise</environment_context>"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"real prompt"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"working"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"exec_command","arguments":"{{\"cmd\":\"pwd\"}}","call_id":"c1"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"function_call_output","output":"done","call_id":"c1"}}}}"#
        )
        .unwrap();
        drop(file);

        let messages = parse_transcript(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0].kind, MessageKind::User));
        assert_eq!(messages[0].content, "real prompt");
        assert!(matches!(messages[1].kind, MessageKind::Assistant));
        assert_eq!(messages[1].content, "working");
        assert!(matches!(messages[2].kind, MessageKind::ToolCall { .. }));
        assert!(matches!(messages[3].kind, MessageKind::ToolOutput));
        assert_eq!(messages[3].content, "done");
    }
}
