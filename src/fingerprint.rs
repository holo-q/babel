//! Session Fingerprinting for Matching Terminals to JSONL Files
//!
//! This module provides fingerprint-based matching for Claude Code sessions.
//! Instead of relying on fragile title matching, we extract semantic patterns
//! from both terminal scrollback and JSONL session files to reliably match them.
//!
//! Design:
//! - Extract first prompt, recent prompts, tool sequences, and CWD from both sources
//! - Normalize prompts for robust comparison (lowercase, trim, truncate)
//! - Score matches using multiple signals (prompts, tools, cwd)
//! - Return confidence level (None/Low/Medium/High/Exact) for filtering
//!
//! Additionally handles project migration when directories are moved:
//! - Renames ~/.claude/projects/{encoded-path} folders
//! - Updates path references in ~/.claude/history.jsonl
//! - Preserves session files and conversation history

use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use serde::{Deserialize, Serialize};
use anyhow::{Result, Context, bail};
use crate::claude_storage::{claude_base, path_to_encoded};

// ═══════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════

/// Fingerprint extracted from scrollback or JSONL for matching
///
/// Captures semantic patterns independent of presentation format:
/// - User prompts (first and recent) for content matching
/// - Tool call sequences for workflow matching
/// - CWD for project context
/// - Timestamp for recency weighting
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionFingerprint {
    /// First user prompt in the session (normalized, max 100 chars)
    /// Strong signal for identifying conversation start
    pub first_prompt: Option<String>,

    /// Last 3 user prompts (normalized, max 100 chars each)
    /// Used for matching active/recent conversations
    pub recent_prompts: Vec<String>,

    /// Ordered sequence of tool names: ["Bash", "Read", "Edit"]
    /// Workflow signature - particularly useful for technical sessions
    pub tool_sequence: Vec<String>,

    /// Working directory extracted from session
    /// Project context signal
    pub cwd: Option<PathBuf>,

    /// Timestamp for recency weighting
    pub timestamp: Option<String>,

    /// Session ID (filled when matched to JSONL)
    pub session_id: Option<String>,
}

/// Match confidence level for fingerprint comparison
///
/// Ordered by strength of evidence - higher scores indicate better matches.
/// Use threshold filtering (e.g., >= Medium) to avoid false positives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MatchConfidence {
    None = 0,      // No meaningful overlap
    Low = 1,       // CWD only (weak - many sessions in same project)
    Medium = 2,    // Single prompt match
    High = 3,      // Multiple prompts or tool sequence match
    Exact = 4,     // First prompt + CWD + tools all match (very confident)
}

// ═══════════════════════════════════════════════════════════════════════════
// Scrollback Extraction
// ═══════════════════════════════════════════════════════════════════════════

/// Extract fingerprint from kitty terminal scrollback
///
/// Looks for patterns:
/// - `> user prompt text` or `╭─` followed by user prompt
/// - `● ToolName(args)` for tool calls
/// - `cwd: /path` in footer
///
/// Scrollback is analyzed bottom-up for recency (recent prompts are at bottom).
pub fn extract_from_scrollback(scrollback: &str) -> SessionFingerprint {
    let line_count = scrollback.lines().count();
    tracing::debug!(scrollback_lines = line_count, "extracting fingerprint from scrollback");

    let mut fingerprint = SessionFingerprint::default();
    let mut user_prompts: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();

    for (line_num, line) in scrollback.lines().enumerate() {
        let trimmed = line.trim();

        // User prompts - multiple patterns
        if let Some(prompt) = extract_user_prompt(trimmed) {
            let normalized = normalize_prompt(&prompt);
            if !normalized.is_empty() {
                tracing::trace!(line_num, prompt = %normalized, "extracted user prompt");
                user_prompts.push(normalized);
            }
        }

        // Tool calls - pattern: ● ToolName(args)
        if let Some(tool) = extract_tool_call(trimmed) {
            tracing::trace!(line_num, tool = %tool, "extracted tool call");
            tools.push(tool);
        }

        // CWD - pattern: cwd: /path or similar
        if let Some(cwd) = extract_cwd(trimmed) {
            tracing::trace!(line_num, ?cwd, "extracted cwd");
            fingerprint.cwd = Some(cwd);
        }
    }

    // Set first prompt (earliest in scrollback)
    fingerprint.first_prompt = user_prompts.first().cloned();

    // Set recent prompts (last 3)
    fingerprint.recent_prompts = user_prompts.iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect();

    // Set tool sequence (preserve order)
    fingerprint.tool_sequence = tools;

    tracing::debug!(
        ?fingerprint.first_prompt,
        prompts_count = fingerprint.recent_prompts.len(),
        tools_count = fingerprint.tool_sequence.len(),
        ?fingerprint.cwd,
        "scrollback extraction complete"
    );

    fingerprint
}

/// Extract user prompt from line if it matches known patterns
///
/// Patterns:
/// - `> prompt text` (Claude Code user message prefix)
/// - `╭─ User` followed by content
/// - Other markers as discovered
///
/// Filters out:
/// - Slash commands (`/command`) - these get expanded before JSONL recording
/// - Empty prompts
fn extract_user_prompt(line: &str) -> Option<String> {
    // Pattern 1: > prompt (Claude Code's user message format)
    if let Some(prompt) = line.strip_prefix('>').map(|s| s.trim()) {
        if !prompt.is_empty() {
            // Skip slash commands - they get expanded before JSONL
            // Slash commands start with / and have no space before the command
            if prompt.starts_with('/') && !prompt.starts_with("/ ") {
                return None;
            }
            return Some(prompt.to_string());
        }
    }

    // Pattern 2: ╭─ User message or similar box drawing
    if line.contains("╭─") && (line.contains("User") || line.contains("user")) {
        // Content typically on next line, but return marker for now
        // This is a simplification - real implementation may need state machine
        return None;
    }

    // Pattern 3: Direct user message indicator
    if line.starts_with("User:") || line.starts_with("user:") {
        let prompt = line.split_once(':').map(|(_, p)| p.trim())?;
        if !prompt.is_empty() {
            // Skip slash commands here too
            if prompt.starts_with('/') && !prompt.starts_with("/ ") {
                return None;
            }
            return Some(prompt.to_string());
        }
    }

    None
}

/// Known Claude Code tool names - used to filter false positives from scrollback
///
/// Claude's terminal output uses bullets for various things (headers, lists).
/// Only these specific tool names should be extracted from scrollback.
const KNOWN_TOOLS: &[&str] = &[
    // Core file operations
    "Read", "Write", "Edit", "Glob", "Grep",
    // Execution
    "Bash", "Task", "TaskOutput", "KillShell",
    // Notebook
    "NotebookEdit",
    // Web
    "WebFetch", "WebSearch",
    // User interaction
    "AskUserQuestion", "TodoWrite",
    // Mode switching
    "EnterPlanMode", "ExitPlanMode",
    // Extensions
    "Skill", "SlashCommand",
];

/// Extract tool call from line if it matches pattern
///
/// Pattern: ● ToolName(args) or similar
/// Only extracts known Claude Code tool names to avoid false positives.
fn extract_tool_call(line: &str) -> Option<String> {
    // Pattern: ● ToolName(...) or • ToolName(...)
    if line.starts_with('●') || line.starts_with('•') {
        let tool_part = line.trim_start_matches('●').trim_start_matches('•').trim();

        // Extract tool name before opening paren
        if let Some(paren_pos) = tool_part.find('(') {
            let tool_name = tool_part[..paren_pos].trim();
            // Validate against known tools
            if KNOWN_TOOLS.contains(&tool_name) {
                return Some(tool_name.to_string());
            }
        }
    }

    None
}

/// Extract CWD from line if it matches pattern
///
/// Patterns:
/// - `cwd: /path`
/// - `Working directory: /path`
///
/// Claude's status bar may include other info after the path, separated by
/// non-breaking spaces or special characters. We stop at the first invalid
/// path character.
fn extract_cwd(line: &str) -> Option<PathBuf> {
    // Pattern: cwd: /path
    if let Some(cwd_part) = line.strip_prefix("cwd:").map(|s| s.trim_start()) {
        let path = extract_path_from_start(cwd_part);
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    // Pattern: Working directory: /path
    if let Some(cwd_part) = line.strip_prefix("Working directory:").map(|s| s.trim_start()) {
        let path = extract_path_from_start(cwd_part);
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    None
}

/// Extract a valid path from the start of a string
///
/// Stops at the first character that's not valid in a path:
/// - Non-breaking space (\u{a0})
/// - Regular space at a non-escaped position
/// - Powerline/special Unicode symbols
/// - Tab or other control characters
fn extract_path_from_start(s: &str) -> &str {
    // Find the first character that terminates the path
    let end = s.char_indices()
        .find(|(_, c)| {
            // Non-breaking space (Claude status bar separator)
            *c == '\u{a0}' ||
            // Tab
            *c == '\t' ||
            // Powerline symbols (U+E0B0-U+E0BF range)
            (*c >= '\u{e0b0}' && *c <= '\u{e0bf}') ||
            // Regular space (unless path is quoted, which we don't handle)
            *c == ' ' ||
            // Control characters
            c.is_control() ||
            // Other common non-path characters that might appear in status
            *c == '│' || *c == '|'
        })
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());

    s[..end].trim_end()
}

// ═══════════════════════════════════════════════════════════════════════════
// JSONL Extraction
// ═══════════════════════════════════════════════════════════════════════════

/// Message content from JSONL (user prompts)
#[derive(Debug, Clone, Deserialize)]
struct UserMessage {
    content: Option<String>,
}

/// Tool use from JSONL (assistant tool calls)
#[derive(Debug, Clone, Deserialize)]
struct ToolUse {
    name: String,
}

/// Content item (can be text or tool_use)
#[derive(Debug, Clone, Deserialize)]
struct ContentItem {
    #[serde(rename = "type")]
    content_type: String,

    #[serde(flatten)]
    data: serde_json::Value,
}

/// Assistant message with potential tool calls
#[derive(Debug, Clone, Deserialize)]
struct AssistantMessage {
    content: Option<Vec<ContentItem>>,
}

/// Generic message entry from JSONL
#[derive(Debug, Clone, Deserialize)]
struct MessageEntry {
    #[serde(rename = "type")]
    entry_type: String,

    message: Option<serde_json::Value>,
    cwd: Option<PathBuf>,
    timestamp: Option<String>,

    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Extract fingerprint from a session JSONL file
///
/// Reads first 50 entries for performance (sessions can be thousands of lines).
/// Extracts user prompts and tool calls in order.
pub fn extract_from_jsonl(path: &Path) -> Result<SessionFingerprint> {
    tracing::debug!(?path, "extracting fingerprint from JSONL");

    let file = File::open(path)
        .with_context(|| format!("Failed to open JSONL file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut fingerprint = SessionFingerprint::default();
    let mut user_prompts: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut lines_parsed = 0;
    let mut lines_skipped = 0;

    // Parse first 50 entries (performance optimization)
    for (line_num, line) in reader.lines().take(50).enumerate() {
        let line = line.with_context(||
            format!("Failed to read line {} from {}", line_num + 1, path.display())
        )?;

        if line.trim().is_empty() {
            lines_skipped += 1;
            continue;
        }

        // Parse as generic entry
        let entry: MessageEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                tracing::trace!(line_num, error = %e, "skipping malformed JSONL line");
                lines_skipped += 1;
                continue;
            }
        };
        lines_parsed += 1;

        tracing::trace!(line_num, entry_type = %entry.entry_type, "parsed JSONL entry");

        // Update metadata
        if fingerprint.cwd.is_none() && entry.cwd.is_some() {
            tracing::debug!(line_num, cwd = ?entry.cwd, "found cwd in JSONL");
            fingerprint.cwd = entry.cwd;
        }
        if fingerprint.timestamp.is_none() && entry.timestamp.is_some() {
            fingerprint.timestamp = entry.timestamp;
        }
        if fingerprint.session_id.is_none() && entry.session_id.is_some() {
            tracing::debug!(line_num, session_id = ?entry.session_id, "found session_id in JSONL");
            fingerprint.session_id = entry.session_id;
        }

        // Extract based on entry type
        match entry.entry_type.as_str() {
            "user" => {
                if let Some(msg_value) = entry.message {
                    if let Ok(user_msg) = serde_json::from_value::<UserMessage>(msg_value) {
                        if let Some(content) = user_msg.content {
                            let normalized = normalize_prompt(&content);
                            if !normalized.is_empty() {
                                tracing::trace!(line_num, prompt = %normalized, "extracted user prompt from JSONL");
                                user_prompts.push(normalized);
                            }
                        }
                    }
                }
            }
            "assistant" => {
                if let Some(msg_value) = entry.message {
                    if let Ok(asst_msg) = serde_json::from_value::<AssistantMessage>(msg_value) {
                        if let Some(content_items) = asst_msg.content {
                            for item in content_items {
                                if item.content_type == "tool_use" {
                                    if let Ok(tool_use) = serde_json::from_value::<ToolUse>(item.data) {
                                        tracing::trace!(line_num, tool = %tool_use.name, "extracted tool from JSONL");
                                        tools.push(tool_use.name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                tracing::trace!(line_num, entry_type = %entry.entry_type, "ignoring non-message entry");
            }
        }
    }

    // Set first prompt
    fingerprint.first_prompt = user_prompts.first().cloned();

    // Set recent prompts (last 3)
    fingerprint.recent_prompts = user_prompts.iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect();

    // Set tool sequence
    fingerprint.tool_sequence = tools;

    tracing::debug!(
        ?path,
        lines_parsed,
        lines_skipped,
        ?fingerprint.first_prompt,
        prompts_count = fingerprint.recent_prompts.len(),
        tools_count = fingerprint.tool_sequence.len(),
        ?fingerprint.cwd,
        ?fingerprint.session_id,
        "JSONL extraction complete"
    );

    Ok(fingerprint)
}

// ═══════════════════════════════════════════════════════════════════════════
// Matching
// ═══════════════════════════════════════════════════════════════════════════

/// Compare two fingerprints and return confidence level
///
/// Scoring logic:
/// - Exact first_prompt match: +2 confidence points
/// - Any recent_prompt match: +1 confidence point
/// - Tool sequence overlap (Jaccard > 0.5): +1 confidence point
/// - CWD match: +1 confidence point (only if both have cwd)
///
/// Mapping to confidence levels:
/// - 0 points: None
/// - 1 point (CWD only): Low
/// - 2 points (1 prompt): Medium
/// - 3 points (2+ signals): High
/// - 4+ points (all signals): Exact
pub fn match_fingerprints(
    scrollback_fp: &SessionFingerprint,
    jsonl_fp: &SessionFingerprint,
) -> MatchConfidence {
    tracing::debug!(
        scrollback_first_prompt = ?scrollback_fp.first_prompt,
        jsonl_first_prompt = ?jsonl_fp.first_prompt,
        jsonl_session_id = ?jsonl_fp.session_id,
        "comparing fingerprints"
    );

    let mut score = 0;
    let mut score_reasons: Vec<&str> = Vec::new();

    // Check first prompt match (strong signal)
    let first_prompt_match = match (&scrollback_fp.first_prompt, &jsonl_fp.first_prompt) {
        (Some(fp1), Some(fp2)) if fp1 == fp2 => {
            score += 2;
            score_reasons.push("first_prompt(+2)");
            true
        }
        (Some(fp1), Some(fp2)) => {
            tracing::debug!(scrollback = %fp1, jsonl = %fp2, "first prompts differ");
            false
        }
        _ => false,
    };

    // Check recent prompts match (any overlap)
    let mut recent_prompt_match = false;
    for scroll_prompt in &scrollback_fp.recent_prompts {
        if jsonl_fp.recent_prompts.contains(scroll_prompt) {
            score += 1;
            score_reasons.push("recent_prompt(+1)");
            recent_prompt_match = true;
            tracing::debug!(prompt = %scroll_prompt, "recent prompt matched");
            break; // Only count once
        }
    }

    // Check tool sequence similarity
    let tool_sim = tool_sequence_similarity(
        &scrollback_fp.tool_sequence,
        &jsonl_fp.tool_sequence,
    );
    let tool_match = tool_sim > 0.5;
    if tool_match {
        score += 1;
        score_reasons.push("tools(+1)");
    }
    tracing::debug!(
        tool_similarity = tool_sim,
        tool_match,
        scrollback_tools = ?scrollback_fp.tool_sequence,
        jsonl_tools = ?jsonl_fp.tool_sequence,
        "tool sequence comparison"
    );

    // Check CWD match (only if both have cwd)
    let cwd_match = match (&scrollback_fp.cwd, &jsonl_fp.cwd) {
        (Some(cwd1), Some(cwd2)) if cwd1 == cwd2 => {
            score += 1;
            score_reasons.push("cwd(+1)");
            true
        }
        (Some(cwd1), Some(cwd2)) => {
            tracing::debug!(?cwd1, ?cwd2, "cwds differ");
            false
        }
        _ => false,
    };

    // Map score to confidence level
    let confidence = match score {
        0 => MatchConfidence::None,
        1 => MatchConfidence::Low,
        2 => MatchConfidence::Medium,
        3 => MatchConfidence::High,
        _ => MatchConfidence::Exact,
    };

    tracing::debug!(
        score,
        ?confidence,
        first_prompt_match,
        recent_prompt_match,
        tool_match,
        cwd_match,
        reasons = ?score_reasons,
        "fingerprint match result"
    );

    confidence
}

// ═══════════════════════════════════════════════════════════════════════════
// Helper Functions
// ═══════════════════════════════════════════════════════════════════════════

/// Normalize prompt text for comparison
///
/// Normalization:
/// - Trim whitespace
/// - Convert to lowercase
/// - Truncate to 100 characters
/// - Remove common noise (multiple spaces, etc.)
fn normalize_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let lowercase = trimmed.to_lowercase();

    // Replace multiple whitespace with single space
    let normalized = lowercase
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Truncate to 100 chars
    if normalized.len() > 100 {
        normalized.chars().take(100).collect()
    } else {
        normalized
    }
}

/// Calculate Jaccard similarity between two tool sequences
///
/// Jaccard = |A ∩ B| / |A ∪ B|
///
/// This measures set overlap regardless of order. For workflow matching,
/// we care more about "did they use similar tools" than exact sequence.
fn tool_sequence_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0; // Both empty = perfect match
    }

    if a.is_empty() || b.is_empty() {
        return 0.0; // One empty = no match
    }

    let set_a: std::collections::HashSet<_> = a.iter().collect();
    let set_b: std::collections::HashSet<_> = b.iter().collect();

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Project Migration
// ═══════════════════════════════════════════════════════════════════════════

/// Result of migrating a project directory
///
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

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_prompt() {
        assert_eq!(
            normalize_prompt("  Fix the BUG  "),
            "fix the bug"
        );

        assert_eq!(
            normalize_prompt("Multiple    spaces     here"),
            "multiple spaces here"
        );

        // Test truncation
        let long_prompt = "a".repeat(150);
        let normalized = normalize_prompt(&long_prompt);
        assert_eq!(normalized.len(), 100);
    }

    #[test]
    fn test_tool_sequence_similarity() {
        let seq_a = vec!["Bash".to_string(), "Read".to_string(), "Edit".to_string()];
        let seq_b = vec!["Bash".to_string(), "Read".to_string(), "Edit".to_string()];
        assert_eq!(tool_sequence_similarity(&seq_a, &seq_b), 1.0);

        let seq_c = vec!["Bash".to_string(), "Read".to_string()];
        let sim = tool_sequence_similarity(&seq_a, &seq_c);
        assert!(sim > 0.5 && sim < 1.0); // 2/3 = 0.666...

        let seq_d = vec!["Grep".to_string(), "Write".to_string()];
        let sim = tool_sequence_similarity(&seq_a, &seq_d);
        assert!(sim < 0.5); // 0/5 = 0.0

        // Empty sequences
        assert_eq!(tool_sequence_similarity(&[], &[]), 1.0);
        assert_eq!(tool_sequence_similarity(&seq_a, &[]), 0.0);
    }

    #[test]
    fn test_extract_user_prompt() {
        // Valid prompts
        assert_eq!(
            extract_user_prompt("> implement the feature"),
            Some("implement the feature".to_string())
        );

        assert_eq!(
            extract_user_prompt("User: fix the bug"),
            Some("fix the bug".to_string())
        );

        // Skip slash commands (they get expanded before JSONL)
        assert_eq!(extract_user_prompt("> /help"), None);
        assert_eq!(extract_user_prompt("> /extra-usage"), None);
        assert_eq!(extract_user_prompt("> /some-command arg"), None);
        assert_eq!(extract_user_prompt("User: /init"), None);

        // But allow paths that happen to start with /
        assert_eq!(
            extract_user_prompt("> / is the root directory"),
            Some("/ is the root directory".to_string())
        );

        // Basic rejections
        assert_eq!(extract_user_prompt("normal text"), None);
        assert_eq!(extract_user_prompt(">"), None); // Empty prompt
    }

    #[test]
    fn test_extract_tool_call() {
        // Valid tool names
        assert_eq!(
            extract_tool_call("● Bash(ls -la)"),
            Some("Bash".to_string())
        );

        assert_eq!(
            extract_tool_call("• Read(/path/to/file.rs)"),
            Some("Read".to_string())
        );

        assert_eq!(
            extract_tool_call("● Edit(src/main.rs)"),
            Some("Edit".to_string())
        );

        // Reject non-tool names (Claude output headings, etc.)
        assert_eq!(extract_tool_call("● Trained Parameters(...)"), None);
        assert_eq!(extract_tool_call("● Key Architectural Insights(...)"), None);
        assert_eq!(extract_tool_call("• Background command(...)"), None);
        assert_eq!(extract_tool_call("● Some Random Thing(...)"), None);

        // Basic rejections
        assert_eq!(extract_tool_call("normal text"), None);
        assert_eq!(extract_tool_call("● NoParens"), None);
    }

    #[test]
    fn test_extract_cwd() {
        // Basic paths
        assert_eq!(
            extract_cwd("cwd: /home/user/project"),
            Some(PathBuf::from("/home/user/project"))
        );

        assert_eq!(
            extract_cwd("Working directory: /tmp"),
            Some(PathBuf::from("/tmp"))
        );

        // With trailing status bar garbage (non-breaking space separator)
        assert_eq!(
            extract_cwd("cwd: /home/nuck/Workspace\u{a0}\u{e0b4}\u{a0}Model:\u{a0}Opus 4.5"),
            Some(PathBuf::from("/home/nuck/Workspace"))
        );

        // With regular space after path
        assert_eq!(
            extract_cwd("cwd: /home/user/project Model: Claude"),
            Some(PathBuf::from("/home/user/project"))
        );

        // Path with dashes and underscores
        assert_eq!(
            extract_cwd("cwd: /home/nuck/some-project_dir"),
            Some(PathBuf::from("/home/nuck/some-project_dir"))
        );

        assert_eq!(extract_cwd("normal text"), None);
    }

    #[test]
    fn test_extract_path_from_start() {
        // Basic path
        assert_eq!(extract_path_from_start("/home/user/project"), "/home/user/project");

        // Path with trailing space
        assert_eq!(extract_path_from_start("/home/user Model: foo"), "/home/user");

        // Path with non-breaking space
        assert_eq!(extract_path_from_start("/home/user\u{a0}extra"), "/home/user");

        // Path with Powerline symbol
        assert_eq!(extract_path_from_start("/path\u{e0b4}status"), "/path");

        // Complex status bar
        assert_eq!(
            extract_path_from_start("/home/nuck/Workspace\u{a0}\u{e0b4}\u{a0}Model:\u{a0}Opus"),
            "/home/nuck/Workspace"
        );
    }

    #[test]
    fn test_extract_from_scrollback() {
        let scrollback = r#"
> implement user authentication
● Bash(ls -la)
● Read(src/auth.rs)
● Edit(src/auth.rs)
cwd: /home/nuck/Workspace
> add tests
● Write(tests/auth_test.rs)
"#;

        let fp = extract_from_scrollback(scrollback);

        assert_eq!(fp.first_prompt, Some("implement user authentication".to_string()));
        assert_eq!(fp.recent_prompts.len(), 2);
        assert!(fp.recent_prompts.contains(&"add tests".to_string()));
        assert_eq!(fp.tool_sequence, vec!["Bash", "Read", "Edit", "Write"]);
        assert_eq!(fp.cwd, Some(PathBuf::from("/home/nuck/Workspace")));
    }

    #[test]
    fn test_match_fingerprints() {
        let mut fp1 = SessionFingerprint::default();
        fp1.first_prompt = Some("fix the bug".to_string());
        fp1.cwd = Some(PathBuf::from("/home/user/project"));
        fp1.tool_sequence = vec!["Bash".to_string(), "Read".to_string()];

        let mut fp2 = SessionFingerprint::default();
        fp2.first_prompt = Some("fix the bug".to_string());
        fp2.cwd = Some(PathBuf::from("/home/user/project"));
        fp2.tool_sequence = vec!["Bash".to_string(), "Read".to_string()];

        // Exact match
        assert_eq!(match_fingerprints(&fp1, &fp2), MatchConfidence::Exact);

        // Only CWD match
        let mut fp3 = SessionFingerprint::default();
        fp3.cwd = Some(PathBuf::from("/home/user/project"));
        assert_eq!(match_fingerprints(&fp1, &fp3), MatchConfidence::Low);

        // First prompt match only
        let mut fp4 = SessionFingerprint::default();
        fp4.first_prompt = Some("fix the bug".to_string());
        assert_eq!(match_fingerprints(&fp1, &fp4), MatchConfidence::Medium);

        // No match
        let fp5 = SessionFingerprint::default();
        assert_eq!(match_fingerprints(&fp1, &fp5), MatchConfidence::None);
    }

    #[test]
    fn test_match_confidence_ordering() {
        assert!(MatchConfidence::None < MatchConfidence::Low);
        assert!(MatchConfidence::Low < MatchConfidence::Medium);
        assert!(MatchConfidence::Medium < MatchConfidence::High);
        assert!(MatchConfidence::High < MatchConfidence::Exact);
    }
}
