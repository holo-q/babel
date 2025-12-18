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
use std::io::{BufRead, BufReader, BufWriter, Write};
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context, bail};

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

/// Get the display name for a session from history.jsonl
///
/// This is the name shown in Claude Code's history panel, which can be set
/// via `/rename <name>` in the Claude Code CLI. Returns None if the session
/// isn't found in history or has no custom name.
pub fn get_session_display_name(session_id: &str) -> Option<String> {
    let history_path = claude_base().join("history.jsonl");

    if !history_path.exists() {
        return None;
    }

    let file = match File::open(&history_path) {
        Ok(f) => f,
        Err(_) => return None,
    };

    let reader = BufReader::new(file);

    // Find the most recent entry for this session (history.jsonl can have duplicates)
    let mut best_match: Option<(i64, String)> = None;

    for line in reader.lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
            if entry.session_id.as_deref() == Some(session_id) {
                // Keep the most recent entry (highest timestamp)
                match &best_match {
                    Some((ts, _)) if entry.timestamp <= *ts => {}
                    _ => best_match = Some((entry.timestamp, entry.display.clone())),
                }
            }
        }
    }

    best_match.map(|(_, display)| display)
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

// ═══════════════════════════════════════════════════════════════════════════
// Project Migration
// ═══════════════════════════════════════════════════════════════════════════

/// Tracks what was changed during migration for user feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrateResult {
    /// Whether the project folder was renamed in ~/.claude/projects/
    pub project_folder_renamed: bool,

    /// Number of entries updated in ~/.claude/history.jsonl
    pub history_entries_updated: usize,

    /// Number of session JSONL files preserved (moved with the folder)
    pub sessions_preserved: usize,

    /// Old project folder name (encoded path)
    pub old_folder: String,

    /// New project folder name (encoded path)
    pub new_folder: String,
}

/// Convert encoded project name back to absolute path
///
/// Example: -home-user-project → /home/user/project
pub fn encoded_to_path(encoded: &str) -> PathBuf {
    // First character is always a dash (from root /), replace all dashes with /
    PathBuf::from(encoded.replace('-', "/"))
}

/// Migrate conversation history when a directory is moved
///
/// This updates Claude Code's internal storage to reflect the new path:
/// 1. Renames the project folder in ~/.claude/projects/
/// 2. Updates all path references in ~/.claude/history.jsonl
///
/// # Arguments
/// * `old_path` - Original absolute path of the directory
/// * `new_path` - New absolute path after the move
/// * `dry_run` - If true, only report what would change without modifying files
///
/// # Returns
/// A `MigrateResult` summarizing what was (or would be) changed.
///
/// # Errors
/// - If old_path and new_path are the same
/// - If new_path is nested inside old_path
/// - If a project folder already exists at the new path (conflict)
pub fn migrate_project(old_path: &Path, new_path: &Path, dry_run: bool) -> Result<MigrateResult> {
    tracing::debug!(?old_path, ?new_path, dry_run, "starting project migration");

    // Canonicalize paths for comparison (resolve symlinks, remove trailing slashes)
    // Old path must exist - canonicalize it
    let old_canonical = old_path.canonicalize()
        .unwrap_or_else(|_| old_path.to_path_buf());
    // New path may not exist yet - make it absolute without requiring existence
    let new_canonical = new_path.canonicalize().unwrap_or_else(|_| {
        if new_path.is_absolute() {
            new_path.to_path_buf()
        } else {
            // Join with cwd to make absolute
            std::env::current_dir()
                .map(|cwd| cwd.join(new_path))
                .unwrap_or_else(|_| new_path.to_path_buf())
        }
    });

    tracing::debug!(?old_canonical, ?new_canonical, "canonicalized paths");

    // Validation: paths must be different
    if old_canonical == new_canonical {
        tracing::debug!("paths are identical, aborting");
        bail!("Source and destination are the same path");
    }

    // Validation: new path cannot be nested inside old path
    if new_canonical.starts_with(&old_canonical) {
        tracing::debug!("new path is nested inside old path, aborting");
        bail!("Destination cannot be nested inside source: {} is inside {}",
            new_path.display(), old_path.display());
    }

    let old_encoded = path_to_encoded(&old_canonical);
    let new_encoded = path_to_encoded(&new_canonical);
    tracing::debug!(%old_encoded, %new_encoded, "encoded folder names");

    let projects_dir = claude_base().join("projects");
    let old_project_dir = projects_dir.join(&old_encoded);
    let new_project_dir = projects_dir.join(&new_encoded);

    tracing::debug!(
        ?old_project_dir,
        old_exists = old_project_dir.exists(),
        ?new_project_dir,
        new_exists = new_project_dir.exists(),
        "checking project directories"
    );

    // Check for conflicts at destination
    if new_project_dir.exists() && !dry_run {
        tracing::debug!("destination project folder already exists, aborting");
        bail!("Project folder already exists at destination: {}\n\
               This would overwrite existing conversation history.\n\
               Either delete the destination folder or use a different path.",
            new_project_dir.display());
    }

    let mut result = MigrateResult {
        project_folder_renamed: false,
        history_entries_updated: 0,
        sessions_preserved: 0,
        old_folder: old_encoded.clone(),
        new_folder: new_encoded.clone(),
    };

    // Step 1: Count sessions and rename project folder
    if old_project_dir.exists() {
        tracing::debug!(?old_project_dir, "project folder exists, counting sessions");

        // Count session files
        if let Ok(entries) = fs::read_dir(&old_project_dir) {
            let sessions: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            result.sessions_preserved = sessions.len();

            for session in &sessions {
                tracing::debug!(session_file = ?session.path(), "found session file");
            }
        }
        tracing::debug!(sessions_count = result.sessions_preserved, "counted session files");

        if !dry_run {
            tracing::debug!(?old_project_dir, ?new_project_dir, "renaming project folder");
            fs::rename(&old_project_dir, &new_project_dir)
                .with_context(|| format!(
                    "Failed to rename project folder: {} → {}",
                    old_project_dir.display(),
                    new_project_dir.display()
                ))?;
            tracing::debug!("project folder renamed successfully");
        } else {
            tracing::debug!("dry run: would rename project folder");
        }
        result.project_folder_renamed = true;
    } else {
        tracing::debug!(?old_project_dir, "project folder does not exist, skipping rename");
    }

    // Step 2: Update history.jsonl
    let history_path = claude_base().join("history.jsonl");
    tracing::debug!(?history_path, exists = history_path.exists(), "checking history.jsonl");

    if history_path.exists() {
        result.history_entries_updated = update_history_paths(
            &history_path,
            &old_canonical,
            &new_canonical,
            dry_run,
        )?;
        tracing::debug!(entries_updated = result.history_entries_updated, "history.jsonl processed");
    } else {
        tracing::debug!("history.jsonl does not exist, skipping");
    }

    tracing::debug!(?result, "migration complete");
    Ok(result)
}

/// Update path references in history.jsonl
///
/// Reads the entire history file, updates any entries where the project
/// path matches old_path (or is a child of old_path), and writes back atomically.
///
/// # Arguments
/// * `history_path` - Path to ~/.claude/history.jsonl
/// * `old_path` - Original path to replace
/// * `new_path` - New path to use
/// * `dry_run` - If true, only count matches without writing
///
/// # Returns
/// Number of entries that were (or would be) updated
fn update_history_paths(
    history_path: &Path,
    old_path: &Path,
    new_path: &Path,
    dry_run: bool,
) -> Result<usize> {
    tracing::debug!(?history_path, ?old_path, ?new_path, dry_run, "updating history paths");

    let file = File::open(history_path)
        .context("Failed to open history.jsonl")?;

    let reader = BufReader::new(file);
    let mut updated_lines = Vec::new();
    let mut update_count = 0;
    let mut total_lines = 0;
    let mut empty_lines = 0;
    let mut malformed_lines = 0;

    let old_path_str = old_path.to_string_lossy();
    let new_path_str = new_path.to_string_lossy();
    tracing::debug!(old_path_str = %old_path_str, new_path_str = %new_path_str, "path strings for matching");

    for (line_num, line) in reader.lines().enumerate() {
        let line = line.context("Failed to read history line")?;
        total_lines += 1;

        if line.trim().is_empty() {
            empty_lines += 1;
            updated_lines.push(line);
            continue;
        }

        // Parse the line as JSON to find "project" field
        if let Ok(mut entry) = serde_json::from_str::<serde_json::Value>(&line) {
            let mut modified = false;

            if let Some(project) = entry.get_mut("project") {
                if let Some(project_str) = project.as_str() {
                    // Check if this entry's project matches old_path exactly
                    // or is a child path (for nested projects)
                    if project_str == old_path_str
                        || project_str.starts_with(&format!("{}/", old_path_str))
                    {
                        // Replace old_path prefix with new_path
                        let new_project = project_str.replacen(&*old_path_str, &new_path_str, 1);
                        tracing::debug!(
                            line_num,
                            old_project = %project_str,
                            new_project = %new_project,
                            "matched history entry, updating path"
                        );
                        *project = serde_json::Value::String(new_project);
                        modified = true;
                        update_count += 1;
                    }
                    // Note: We intentionally don't log non-matches to avoid thousands of log lines
                }
            }

            if modified {
                updated_lines.push(serde_json::to_string(&entry)?);
            } else {
                updated_lines.push(line);
            }
        } else {
            // Keep malformed lines as-is
            malformed_lines += 1;
            tracing::trace!(line_num, "malformed JSON line, keeping as-is");
            updated_lines.push(line);
        }
    }

    tracing::debug!(
        total_lines,
        empty_lines,
        malformed_lines,
        update_count,
        "finished scanning history.jsonl"
    );

    if !dry_run && update_count > 0 {
        // Write atomically: temp file + rename
        let temp_path = history_path.with_extension("jsonl.tmp");
        tracing::debug!(?temp_path, "writing updated history to temp file");

        let temp_file = File::create(&temp_path)
            .context("Failed to create temp history file")?;
        let mut writer = BufWriter::new(temp_file);

        for line in &updated_lines {
            writeln!(writer, "{}", line)?;
        }
        writer.flush()?;

        tracing::debug!(?temp_path, ?history_path, "atomically replacing history.jsonl");
        fs::rename(&temp_path, history_path)
            .context("Failed to replace history.jsonl")?;
        tracing::debug!("history.jsonl updated successfully");
    } else if dry_run && update_count > 0 {
        tracing::debug!(update_count, "dry run: would update {} entries", update_count);
    } else {
        tracing::debug!("no entries to update");
    }

    Ok(update_count)
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
