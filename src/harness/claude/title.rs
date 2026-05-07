use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;

use crate::title_policy::session_title::SessionTitleHarness;

pub(crate) struct TitleHarness;

#[async_trait]
impl SessionTitleHarness for TitleHarness {
    async fn read_user_prompts(&self, native_id: &str) -> Result<Vec<String>> {
        let path = crate::utility::claude_storage::find_session_transcript(native_id)?
            .with_context(|| format!("Claude transcript not found for {native_id}"))?;
        read_user_prompts(&path)
    }

    async fn write_title(
        &self,
        native_id: &str,
        title: &str,
        settle_delay: Duration,
    ) -> Result<()> {
        let path = crate::utility::claude_storage::find_session_transcript(native_id)?
            .with_context(|| format!("Claude transcript not found for {native_id}"))?;
        write_custom_title(&path, title, settle_delay).await
    }
}

fn read_user_prompts(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open Claude transcript {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut prompts = Vec::new();

    for line in reader.lines().map_while(|line| line.ok()) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|value| value.as_str()) != Some("user") {
            continue;
        }
        if let Some(text) = message_text(value.get("message")) {
            if !text.trim().is_empty() {
                prompts.push(text);
            }
        }
    }

    Ok(prompts)
}

fn message_text(message: Option<&serde_json::Value>) -> Option<String> {
    let content = message?.get("content")?;
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(|value| value.as_str()) == Some("text"))
                .filter_map(|block| block.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

async fn write_custom_title(jsonl_path: &Path, title: &str, settle_delay: Duration) -> Result<()> {
    tokio::time::sleep(settle_delay).await;
    if !jsonl_path.exists() {
        anyhow::bail!(
            "Claude transcript not found for title write: {}",
            jsonl_path.display()
        );
    }

    let content = tokio::fs::read_to_string(jsonl_path)
        .await
        .with_context(|| format!("read Claude transcript {}", jsonl_path.display()))?;
    let entry = serde_json::to_string(&json!({
        "type": "custom-title",
        "customTitle": title,
    }))?;

    let mut replaced = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        let is_custom_title = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|kind| kind.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("custom-title");

        if is_custom_title {
            if !replaced {
                lines.push(entry.clone());
                replaced = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(entry);
    }

    atomic_write_lines(jsonl_path, lines).await
}

async fn atomic_write_lines(path: &Path, lines: Vec<String>) -> Result<()> {
    let mut content = lines.join("\n");
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let parent = path.parent().context("JSONL has no parent directory")?;
    let temp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    tokio::fs::write(&temp_path, content)
        .await
        .with_context(|| format!("write temp {}", temp_path.display()))?;
    tokio::fs::rename(&temp_path, path)
        .await
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_extractor_reads_text_blocks() {
        let value = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_result", "content": "ignored"},
                {"type": "text", "text": "world"}
            ]
        });

        assert_eq!(message_text(Some(&value)), Some("hello\nworld".to_string()));
    }

    #[tokio::test]
    async fn custom_title_write_replaces_existing_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        tokio::fs::write(
            &path,
            [
                serde_json::json!({"type": "user", "message": {"content": "hello"}}).to_string(),
                serde_json::json!({"type": "custom-title", "customTitle": "old"}).to_string(),
                serde_json::json!({"type": "assistant", "message": {"content": "hi"}}).to_string(),
            ]
            .join("\n"),
        )
        .await
        .unwrap();

        write_custom_title(&path, "new", Duration::from_millis(0))
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains(r#""type":"custom-title""#));
        assert!(content.contains(r#""customTitle":"new""#));
        assert!(!content.contains(r#""customTitle":"old""#));
        assert_eq!(content.matches("custom-title").count(), 1);
    }
}
