use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use scrollparse::MessageKind;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::title_policy::session_title::SessionTitleHarness;

pub(crate) struct TitleHarness;

#[async_trait]
impl SessionTitleHarness for TitleHarness {
    async fn read_user_prompts(&self, native_id: &str) -> Result<Vec<String>> {
        let path = crate::harness::codex::transcript::find_session_transcript(native_id)?
            .with_context(|| format!("Codex rollout not found for {native_id}"))?;
        let messages = crate::harness::codex::transcript::parse_transcript(&path)?;
        Ok(messages
            .into_iter()
            .filter_map(|message| {
                if matches!(message.kind, MessageKind::User) {
                    Some(message.content)
                } else {
                    None
                }
            })
            .collect())
    }

    async fn write_title(
        &self,
        native_id: &str,
        title: &str,
        _settle_delay: Duration,
    ) -> Result<()> {
        write_thread_title(native_id, title).await
    }
}

async fn write_thread_title(native_id: &str, title: &str) -> Result<()> {
    let codex_home = dirs::home_dir().context("no home dir")?.join(".codex");
    let rollout = crate::harness::codex::transcript::find_session_transcript(native_id)?
        .with_context(|| format!("Codex rollout not found for {native_id}"))?;

    let event = json!({
        "type": "event_msg",
        "payload": {
            "type": "thread_name_updated",
            "thread_id": native_id,
            "thread_name": title,
        }
    });
    append_jsonl(&rollout, &event).await?;

    let index_entry = json!({
        "id": native_id,
        "thread_name": title,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });
    append_jsonl(&codex_home.join("session_index.jsonl"), &index_entry).await?;
    update_state_db_title(&codex_home, native_id, title)?;
    Ok(())
}

async fn append_jsonl(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open append {}", path.display()))?;
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

fn update_state_db_title(codex_home: &Path, native_id: &str, title: &str) -> Result<()> {
    let Some(db_path) = find_state_db(codex_home) else {
        return Ok(());
    };
    let mut conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("open Codex state DB {}", db_path.display()))?;
    let tx = conn.transaction()?;
    let has_title: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads') WHERE name='title')",
        [],
        |row| row.get(0),
    )?;
    if has_title {
        tx.execute(
            "UPDATE threads SET title = ?1 WHERE id = ?2",
            rusqlite::params![title, native_id],
        )?;
    }
    tx.commit()?;
    Ok(())
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
