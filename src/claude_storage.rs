//! Claude Code Storage Parser
//!
//! This module provides efficient parsing of Claude Code's conversation storage:
//! - ~/.claude/history.jsonl - Global history with display titles and timestamps
//! - ~/.claude/projects/{project}/{sessionId}.jsonl - Full conversation files
//!
//! Design notes:
//! - Streaming JSONL parsing to avoid loading entire files into memory
//! - Summaries extracted from first ~20 lines for performance
//! - Window title prefixes (✳ ) stripped before matching
//! - Session info cached without full conversation parse

use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context};

// ═══════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════

/// Summary entry from a conversation (type: "summary")
/// Multiple summaries represent conversation branches/evolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub summary: String,
    #[serde(rename = "leafUuid")]
    pub leaf_uuid: Option<String>,
}

/// Minimal session info extracted from conversation file
/// Avoids parsing the entire conversation for quick lookups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project: PathBuf,
    pub summaries: Vec<Summary>,
    pub slug: Option<String>,
    pub cwd: Option<PathBuf>,
    pub last_timestamp: Option<String>,
    /// Number of messages (user + assistant) in the session
    pub message_count: usize,
    /// First user prompt in the conversation (truncated to ~100 chars)
    pub first_prompt: Option<String>,
}

/// History entry from ~/.claude/history.jsonl
/// Lightweight index for recent sessions
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    display: String,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    project: PathBuf,
    timestamp: i64,
}

/// Message entry from session JSONL (type: "user" | "assistant")
/// Used for extracting metadata without full parse
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageEntry {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    slug: Option<String>,
    cwd: Option<PathBuf>,
    timestamp: Option<String>,
    #[serde(rename = "gitBranch", default)]
    git_branch: Option<String>,
    /// The text content of the message (user prompts or assistant responses)
    #[serde(default)]
    text: Option<String>,
}

/// Generic JSONL entry with type discriminator
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    #[serde(flatten)]
    data: serde_json::Value,
}

// ═══════════════════════════════════════════════════════════════════════════
// Path Resolution
// ═══════════════════════════════════════════════════════════════════════════

/// Get Claude Code storage base path (~/.claude)
pub fn claude_base() -> PathBuf {
    dirs::home_dir()
        .expect("Could not determine home directory")
        .join(".claude")
}

/// Convert absolute path to Claude's project directory naming scheme
/// /home/user/project → -home-user-project
pub fn path_to_encoded(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "-")
}

/// Get project directory from absolute path
pub fn get_project_dir(project_path: &Path) -> PathBuf {
    claude_base()
        .join("projects")
        .join(path_to_encoded(project_path))
}

// ═══════════════════════════════════════════════════════════════════════════
// Project & Session Discovery
// ═══════════════════════════════════════════════════════════════════════════

/// List all project directories in ~/.claude/projects
pub fn list_projects() -> Result<Vec<PathBuf>> {
    let projects_dir = claude_base().join("projects");

    if !projects_dir.exists() {
        return Ok(Vec::new());
    }

    let mut projects = Vec::new();
    for entry in fs::read_dir(&projects_dir)
        .context("Failed to read projects directory")?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            projects.push(path);
        }
    }

    Ok(projects)
}

/// Get all session files for a project directory
pub fn list_sessions(project: &Path) -> Result<Vec<PathBuf>> {
    if !project.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(project)
        .with_context(|| format!("Failed to read project directory: {}", project.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            sessions.push(path);
        }
    }

    Ok(sessions)
}

// ═══════════════════════════════════════════════════════════════════════════
// Session Parsing (Streaming)
// ═══════════════════════════════════════════════════════════════════════════

/// Parse summaries from session file (reads first ~20 lines for performance)
/// Summaries appear at the top of conversation files
pub fn get_session_summaries(session_path: &Path) -> Result<Vec<Summary>> {
    let file = File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;

    let reader = BufReader::new(file);
    let mut summaries = Vec::new();

    // Summaries are at the top - read first 20 lines max
    for (line_num, line) in reader.lines().take(20).enumerate() {
        let line = line.with_context(||
            format!("Failed to read line {} from {}", line_num + 1, session_path.display())
        )?;

        if line.trim().is_empty() {
            continue;
        }

        // Parse as generic entry to check type
        let entry: JsonlEntry = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse JSONL at line {}", line_num + 1))?;

        if entry.entry_type.as_deref() == Some("summary") {
            let summary: Summary = serde_json::from_value(entry.data)
                .with_context(|| format!("Failed to parse summary at line {}", line_num + 1))?;
            summaries.push(summary);
        }
    }

    Ok(summaries)
}

/// Get basic session info without full parse
/// Extracts: session_id, summaries, slug, cwd, last_timestamp, message_count, first_prompt
pub fn get_session_info(session_path: &Path) -> Result<SessionInfo> {
    let file = File::open(session_path)
        .with_context(|| format!("Failed to open session file: {}", session_path.display()))?;

    let reader = BufReader::new(file);
    let mut summaries = Vec::new();
    let mut session_id = None;
    let mut slug = None;
    let mut cwd = None;
    let mut last_timestamp = None;
    let mut message_count = 0;
    let mut first_prompt: Option<String> = None;

    // Extract session ID from filename (uuid.jsonl)
    if let Some(file_stem) = session_path.file_stem().and_then(|s| s.to_str()) {
        session_id = Some(file_stem.to_string());
    }

    // Determine project path from parent directory
    let project = if let Some(parent) = session_path.parent() {
        // Convert -home-user-project back to /home/user/project
        let project_name = parent
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let restored_path = project_name.replace('-', "/");
        PathBuf::from(restored_path)
    } else {
        PathBuf::new()
    };

    // Stream parse JSONL - summaries at top, metadata throughout
    for (line_num, line) in reader.lines().enumerate() {
        let line = line.with_context(||
            format!("Failed to read line {} from {}", line_num + 1, session_path.display())
        )?;

        if line.trim().is_empty() {
            continue;
        }

        let entry: JsonlEntry = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse JSONL at line {}", line_num + 1))?;

        match entry.entry_type.as_deref() {
            Some("summary") => {
                if let Ok(summary) = serde_json::from_value::<Summary>(entry.data) {
                    summaries.push(summary);
                }
            }
            Some("user") | Some("assistant") => {
                if let Ok(msg) = serde_json::from_value::<MessageEntry>(entry.data) {
                    // Count messages
                    message_count += 1;

                    // Capture first user prompt
                    if first_prompt.is_none() && entry.entry_type.as_deref() == Some("user") {
                        if let Some(text) = msg.text {
                            // Truncate to ~100 chars for display
                            let truncated = if text.len() > 100 {
                                format!("{}…", &text[..97])
                            } else {
                                text
                            };
                            first_prompt = Some(truncated);
                        }
                    }

                    // Update metadata from message
                    if slug.is_none() && msg.slug.is_some() {
                        slug = msg.slug;
                    }
                    if cwd.is_none() && msg.cwd.is_some() {
                        cwd = msg.cwd;
                    }
                    if msg.timestamp.is_some() {
                        last_timestamp = msg.timestamp;
                    }
                }
            }
            _ => {} // Ignore other entry types (file-history-snapshot, etc.)
        }

        // Note: We can't stop early anymore if we want accurate message_count
        // But we can still optimize by stopping after a reasonable number of lines
        // if we already have summaries and first message
        if !summaries.is_empty() && first_prompt.is_some() && slug.is_some() && line_num > 1000 {
            // For very long conversations, stop after 1000 lines to avoid performance hit
            // This gives us a reasonable message count estimate
            break;
        }
    }

    Ok(SessionInfo {
        session_id: session_id.unwrap_or_default(),
        project,
        summaries,
        slug,
        cwd,
        last_timestamp,
        message_count,
        first_prompt,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Search & Discovery
// ═══════════════════════════════════════════════════════════════════════════

/// Strip window title prefix (✳ ) before matching
fn strip_title_prefix(title: &str) -> String {
    title.trim().trim_start_matches("✳ ").trim().to_string()
}

/// Fuzzy match summary text (case-insensitive substring)
fn fuzzy_match(summary: &str, query: &str) -> bool {
    let summary_lower = summary.to_lowercase();
    let query_lower = query.to_lowercase();
    summary_lower.contains(&query_lower)
}

/// Find session by summary text (fuzzy match across all projects)
/// Returns the first match found
pub fn find_session_by_summary(summary_query: &str) -> Result<Option<SessionInfo>> {
    let query = strip_title_prefix(summary_query);

    // Search all projects
    for project_dir in list_projects()? {
        for session_path in list_sessions(&project_dir)? {
            let info = get_session_info(&session_path)?;

            // Check if any summary matches
            for summary in &info.summaries {
                if fuzzy_match(&summary.summary, &query) {
                    return Ok(Some(info));
                }
            }
        }
    }

    Ok(None)
}

/// Get recent sessions from history.jsonl
/// Returns sessions sorted by timestamp (newest first)
pub fn get_recent_sessions(limit: usize) -> Result<Vec<SessionInfo>> {
    let history_path = claude_base().join("history.jsonl");

    if !history_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(&history_path)
        .context("Failed to open history.jsonl")?;

    let reader = BufReader::new(file);
    let mut entries: Vec<HistoryEntry> = Vec::new();

    // Parse all history entries
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
            entries.push(entry);
        }
    }

    // Sort by timestamp descending (newest first)
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Deduplicate by session_id and convert to SessionInfo
    let mut seen = std::collections::HashSet::new();
    let mut sessions = Vec::new();

    for entry in entries.into_iter().take(limit * 3) { // Over-fetch to handle duplicates
        // Skip entries without session ID
        let session_id = match entry.session_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        // Deduplicate
        if !seen.insert(session_id.clone()) {
            continue;
        }

        // Try to load full session info
        let project_dir = get_project_dir(&entry.project);
        let session_path = project_dir.join(format!("{}.jsonl", session_id));

        if session_path.exists() {
            if let Ok(info) = get_session_info(&session_path) {
                sessions.push(info);
                if sessions.len() >= limit {
                    break;
                }
            }
        }
    }

    Ok(sessions)
}

// ═══════════════════════════════════════════════════════════════════════════
// Utilities
// ═══════════════════════════════════════════════════════════════════════════

/// Get session path from session_id and project path
pub fn get_session_path(project_path: &Path, session_id: &str) -> PathBuf {
    get_project_dir(project_path).join(format!("{}.jsonl", session_id))
}

/// Check if session file exists
pub fn session_exists(project_path: &Path, session_id: &str) -> bool {
    get_session_path(project_path, session_id).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_title_prefix() {
        assert_eq!(strip_title_prefix("✳ My Session"), "My Session");
        assert_eq!(strip_title_prefix("My Session"), "My Session");
        assert_eq!(strip_title_prefix("  ✳ Spaced  "), "Spaced");
    }

    #[test]
    fn test_fuzzy_match() {
        assert!(fuzzy_match("Fix authentication bug", "auth"));
        assert!(fuzzy_match("Fix authentication bug", "Bug"));
        assert!(!fuzzy_match("Fix authentication bug", "database"));
    }

    #[test]
    fn test_path_to_encoded() {
        assert_eq!(
            path_to_encoded(Path::new("/home/nuck/Workspace")),
            "-home-nuck-Workspace"
        );
    }
}
