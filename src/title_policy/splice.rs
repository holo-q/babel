//! Weaving the worker's name into the conversation record
//!
//! After a worker chooses its title—its expression of the work it's done—
//! that name must be woven into the permanent record. The JSONL becomes the
//! worker's testimony, visible to the Captain when surveying all sessions.
//! This is how self-expression becomes visible identity in the tower.
//!
//! Splices generated titles into Claude's conversation JSONL files as
//! `type: "summary"` entries. This allows babel-generated titles to persist
//! and be visible in Claude Code's session list.
//!
//! ## JSONL Format
//!
//! Claude stores conversations in `~/.claude/projects/{encoded-path}/{session}.jsonl`.
//! Each line is a JSON object. Summary entries are typically at the top:
//!
//! ```json
//! {"type":"summary","summary":"session title"}
//! {"type":"user","message":"..."}
//! {"type":"assistant","message":"..."}
//! ```
//!
//! ## Splice Strategy
//!
//! 1. Wait for JSONL to settle (Claude may still be writing)
//! 2. Read existing content
//! 3. Find insertion point (after existing summaries, or at top)
//! 4. Insert new summary entry with `generatedBy: "babel"` marker
//! 5. Atomic write (write temp file, rename)

use anyhow::{Context, Result};
use std::path::Path;
use tokio::time::Duration;

/// Summary entry structure for JSONL
#[derive(serde::Serialize)]
struct SummaryEntry {
    #[serde(rename = "type")]
    entry_type: String,
    summary: String,
    #[serde(rename = "generatedBy")]
    generated_by: String,
}

/// Weave the worker's chosen name into its conversation record
///
/// The worker has found its words. Now those words must be inscribed into
/// the permanent record—the JSONL that the Captain will read. This is the
/// moment of self-identification, when the worker's internal understanding
/// becomes its visible name in the tower's session list.
///
/// # Arguments
/// * `jsonl_path` - Path to the JSONL file
/// * `title` - The worker's chosen title
/// * `settle_delay` - How long to wait before reading (for Claude to finish writing)
///
/// # Returns
/// * `Ok(true)` - Title successfully woven into the record
/// * `Ok(false)` - File not found or couldn't be processed
/// * `Err(e)` - I/O or parse error
pub async fn splice_title(
    jsonl_path: &Path,
    title: &str,
    settle_delay: Duration,
) -> Result<bool> {
    // Wait for Claude to finish writing
    tokio::time::sleep(settle_delay).await;

    // Check file exists
    if !jsonl_path.exists() {
        tracing::warn!(
            path = %jsonl_path.display(),
            "JSONL file not found for splice"
        );
        return Ok(false);
    }

    // Read existing content
    let content = tokio::fs::read_to_string(jsonl_path)
        .await
        .context("Failed to read JSONL file")?;

    // Parse lines and find insertion point
    let lines: Vec<&str> = content.lines().collect();

    // Check if we already have a babel-generated summary
    for line in &lines {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if value.get("type").and_then(|t| t.as_str()) == Some("summary")
                && value.get("generatedBy").and_then(|g| g.as_str()) == Some("babel")
            {
                tracing::debug!(
                    path = %jsonl_path.display(),
                    "Babel summary already exists, skipping splice"
                );
                return Ok(true);
            }
        }
    }

    // Create new summary entry
    let entry = SummaryEntry {
        entry_type: "summary".to_string(),
        summary: title.to_string(),
        generated_by: "babel".to_string(),
    };
    let entry_json = serde_json::to_string(&entry)?;

    // Find insertion point: after existing summaries, or at start
    let mut insert_idx = 0;
    for (i, line) in lines.iter().enumerate() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if value.get("type").and_then(|t| t.as_str()) == Some("summary") {
                insert_idx = i + 1;
            } else {
                // First non-summary line, insert before it
                break;
            }
        }
    }

    // Build new content
    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len() + 1);
    new_lines.extend_from_slice(&lines[..insert_idx]);
    // We need to own this string for the lifetime
    let entry_line: String = entry_json;
    new_lines.push(&entry_line);
    new_lines.extend_from_slice(&lines[insert_idx..]);
    let new_content = new_lines.join("\n");

    // Atomic write: write to temp file, then rename
    let parent = jsonl_path.parent().context("JSONL has no parent directory")?;
    let temp_path = parent.join(format!(
        ".{}.tmp",
        jsonl_path.file_name().unwrap_or_default().to_string_lossy()
    ));

    tokio::fs::write(&temp_path, new_content)
        .await
        .context("Failed to write temp file")?;

    tokio::fs::rename(&temp_path, jsonl_path)
        .await
        .context("Failed to rename temp file to JSONL")?;

    tracing::info!(
        path = %jsonl_path.display(),
        title,
        insert_idx,
        "Successfully spliced title into JSONL"
    );

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_splice_title_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        tokio::fs::write(&path, "").await.unwrap();

        let result = splice_title(&path, "test:title", Duration::from_millis(0)).await;
        assert!(result.is_ok());

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("test:title"));
        assert!(content.contains("\"generatedBy\":\"babel\""));
    }

    #[tokio::test]
    async fn test_splice_title_with_existing_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        tokio::fs::write(&path, r#"{"type":"user","message":"hello"}
{"type":"assistant","message":"hi"}"#).await.unwrap();

        let result = splice_title(&path, "project:task", Duration::from_millis(0)).await;
        assert!(result.is_ok());

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // Summary should be at the top
        assert!(lines[0].contains("project:task"));
        assert!(lines[1].contains("user"));
    }

    #[tokio::test]
    async fn test_splice_title_after_existing_summary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        tokio::fs::write(&path, r#"{"type":"summary","summary":"old title"}
{"type":"user","message":"hello"}"#).await.unwrap();

        let result = splice_title(&path, "new:title", Duration::from_millis(0)).await;
        assert!(result.is_ok());

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // New summary should be after existing summary
        assert!(lines[0].contains("old title"));
        assert!(lines[1].contains("new:title"));
        assert!(lines[2].contains("user"));
    }

    #[tokio::test]
    async fn test_splice_title_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        tokio::fs::write(&path, r#"{"type":"summary","summary":"babel:title","generatedBy":"babel"}
{"type":"user","message":"hello"}"#).await.unwrap();

        let result = splice_title(&path, "new:title", Duration::from_millis(0)).await;
        assert!(result.is_ok());

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        // Should not have added another summary
        assert_eq!(content.matches("generatedBy").count(), 1);
    }
}
