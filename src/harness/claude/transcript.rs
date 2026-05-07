//! Claude JSONL Session Parser
//!
//! Parses ~/.claude/projects/{project}/{session}.jsonl into scrollparse::Message stream.
//! Reuses scrollparse::MessageKind for consistent rendering.
//!
//! Lives under `harness::claude` — the harness owns its own transcript format,
//! keeping pager/UI free of harness-specific storage details.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Result;
use scrollparse::{Message, MessageKind};
use serde::Deserialize;

/// Content block within a message
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[allow(dead_code)]
        signature: Option<String>,
    },
    ToolUse {
        name: String,
        input: serde_json::Value,
        #[allow(dead_code)]
        id: Option<String>,
    },
    ToolResult {
        content: String,
        #[allow(dead_code)]
        tool_use_id: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// Nested message structure from Claude API
#[derive(Debug, Deserialize)]
struct NestedMessage {
    role: String,
    content: ContentValue,
}

/// Content can be a string or array of blocks
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContentValue {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Raw JSONL entry from agent session file
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlEntry {
    Summary {
        summary: String,
        #[serde(rename = "leafUuid")]
        #[allow(dead_code)]
        leaf_uuid: Option<String>,
    },
    User {
        message: NestedMessage,
        #[allow(dead_code)]
        timestamp: Option<String>,
    },
    Assistant {
        message: NestedMessage,
        #[allow(dead_code)]
        timestamp: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// Parse a session JSONL file into a vector of Messages
///
/// Reads the file line by line, extracting user and assistant messages.
/// Summaries are skipped (handled separately by claude_storage).
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

        // Parse JSONL entry
        let entry: JsonlEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue, // Skip malformed lines
        };

        match entry {
            JsonlEntry::User { message, .. } => {
                let content = extract_content(&message.content);
                if !content.is_empty() {
                    messages.push(Message {
                        kind: MessageKind::User,
                        content,
                        line: line_num,
                    });
                }
            }
            JsonlEntry::Assistant { message, .. } => {
                // Extract text and tool calls from content blocks
                match message.content {
                    ContentValue::Text(text) if !text.is_empty() => {
                        messages.push(Message {
                            kind: MessageKind::Assistant,
                            content: text,
                            line: line_num,
                        });
                    }
                    ContentValue::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                ContentBlock::Text { text } if !text.is_empty() => {
                                    messages.push(Message {
                                        kind: MessageKind::Assistant,
                                        content: text,
                                        line: line_num,
                                    });
                                }
                                ContentBlock::ToolUse { name, input, .. } => {
                                    let args = serde_json::to_string(&input).unwrap_or_default();
                                    messages.push(Message {
                                        kind: MessageKind::ToolCall { name, args },
                                        content: String::new(),
                                        line: line_num,
                                    });
                                }
                                ContentBlock::ToolResult { content, .. } if !content.is_empty() => {
                                    messages.push(Message {
                                        kind: MessageKind::ToolOutput,
                                        content,
                                        line: line_num,
                                    });
                                }
                                // Skip thinking blocks in output (internal reasoning)
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(messages)
}

/// Extract text content from ContentValue
fn extract_content(content: &ContentValue) -> String {
    match content {
        ContentValue::Text(text) => text.clone(),
        ContentValue::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_message() {
        let json = r#"{"type": "user", "message": {"role": "user", "content": "Hello!"}}"#;
        let entry: JsonlEntry = serde_json::from_str(json).unwrap();
        match entry {
            JsonlEntry::User { message, .. } => {
                let content = extract_content(&message.content);
                assert_eq!(content, "Hello!");
            }
            _ => panic!("Expected User entry"),
        }
    }

    #[test]
    fn test_parse_assistant_text() {
        let json = r#"{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Hi there!"}]}}"#;
        let entry: JsonlEntry = serde_json::from_str(json).unwrap();
        match entry {
            JsonlEntry::Assistant { message, .. } => {
                let content = extract_content(&message.content);
                assert_eq!(content, "Hi there!");
            }
            _ => panic!("Expected Assistant entry"),
        }
    }

    #[test]
    fn test_parse_tool_use() {
        let json = r#"{"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "name": "Read", "input": {"path": "/foo"}, "id": "123"}]}}"#;
        let entry: JsonlEntry = serde_json::from_str(json).unwrap();
        assert!(matches!(entry, JsonlEntry::Assistant { .. }));
    }
}
