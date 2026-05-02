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
//! Tracing:
//! - All public functions have #[instrument] for entry/exit logging
//! - Use RUST_LOG=babel=debug for detailed fingerprint diagnostics

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tracing::instrument;

// VTR semantic tracing macros for fingerprint extraction
// - checkpoint!: Milestones reached (fingerprint extraction phases)
// - scope!: Named regions grouping major operations
// Note: match_fingerprints uses #[vtr::branches] for auto-injected decision! calls
use vtr::{checkpoint, scope};

// ═══════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════

/// The soul's signature—traces left by a conversation's passage through time
///
/// Each session leaves unique marks as it flows through terminals and logs.
/// These patterns—opening words, rhythms of action, the ground it walks upon—
/// become how we recognize which conversation inhabits which vessel.
///
/// Captures semantic patterns independent of presentation format:
/// - User prompts (first and recent) for content matching
/// - Tool call sequences for workflow matching
/// - CWD for project context
/// - Timestamp for recency weighting
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionFingerprint {
    /// The opening words, etched in memory
    ///
    /// First user prompt in the session (normalized, max 100 chars).
    /// Every conversation begins somewhere—this is where the soul first spoke.
    /// Strong signal for identifying conversation start.
    pub first_prompt: Option<String>,

    /// Last 3 user prompts (normalized, max 100 chars each)
    /// Used for matching active/recent conversations
    pub recent_prompts: Vec<String>,

    /// The pattern of actions, a behavioral fingerprint
    ///
    /// Ordered sequence of tool names: ["Bash", "Read", "Edit"]
    /// How a conversation moves through the world reveals its nature—
    /// the rhythm of Read then Edit, the cadence of Bash then Grep.
    /// Workflow signature particularly useful for technical sessions.
    pub tool_sequence: Vec<String>,

    /// Working directory extracted from session
    /// Project context signal
    pub cwd: Option<PathBuf>,

    /// Timestamp for recency weighting
    pub timestamp: Option<String>,

    /// Session ID (filled when matched to JSONL)
    pub session_id: Option<String>,
}

/// How certain we are this is the same spirit
///
/// When two fingerprints meet, we measure their resonance—how many traces align,
/// how deep the recognition goes. A stranger shares our workspace (Low). A familiar
/// voice echoes one remembered phrase (Medium). But when opening words, walking paths,
/// and action patterns all align? That's when we know: the same soul, returned.
///
/// Ordered by strength of evidence - higher scores indicate better matches.
/// Use threshold filtering (e.g., >= Medium) to avoid false positives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MatchConfidence {
    None = 0,   // No meaningful overlap—a stranger's marks
    Low = 1,    // CWD only (weak - many sessions in same project)
    Medium = 2, // Single prompt match—one phrase remembered
    High = 3,   // Multiple prompts or tool sequence match—familiar patterns emerge
    Exact = 4,  // First prompt + CWD + tools all match—unmistakable recognition
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
#[instrument(skip(scrollback), fields(scrollback_len = scrollback.len()))]
pub fn extract_from_scrollback(scrollback: &str) -> SessionFingerprint {
    scope!("scrollback_extraction", {
        let line_count = scrollback.lines().count();
        checkpoint!("scrollback_start", lines = line_count);

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
        fingerprint.recent_prompts = user_prompts.iter().rev().take(3).rev().cloned().collect();

        // Set tool sequence (preserve order)
        fingerprint.tool_sequence = tools;

        checkpoint!(
            "scrollback_complete",
            first_prompt = format!("{:?}", fingerprint.first_prompt),
            prompts = fingerprint.recent_prompts.len(),
            tools = fingerprint.tool_sequence.len(),
            cwd = format!("{:?}", fingerprint.cwd)
        );

        fingerprint
    })
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
#[instrument(level = "trace", skip(line), fields(line_len = line.len()))]
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
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    // Execution
    "Bash",
    "Task",
    "TaskOutput",
    "KillShell",
    // Notebook
    "NotebookEdit",
    // Web
    "WebFetch",
    "WebSearch",
    // User interaction
    "AskUserQuestion",
    "TodoWrite",
    // Mode switching
    "EnterPlanMode",
    "ExitPlanMode",
    // Extensions
    "Skill",
    "SlashCommand",
];

/// Extract tool call from line if it matches pattern
///
/// Pattern: ● ToolName(args) or similar
/// Only extracts known Claude Code tool names to avoid false positives.
#[instrument(level = "trace", skip(line))]
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
#[instrument(level = "trace", skip(line))]
fn extract_cwd(line: &str) -> Option<PathBuf> {
    // Pattern: cwd: /path
    if let Some(cwd_part) = line.strip_prefix("cwd:").map(|s| s.trim_start()) {
        let path = extract_path_from_start(cwd_part);
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    // Pattern: Working directory: /path
    if let Some(cwd_part) = line
        .strip_prefix("Working directory:")
        .map(|s| s.trim_start())
    {
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
    let end = s
        .char_indices()
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
#[instrument(fields(path = %path.display()))]
pub fn extract_from_jsonl(path: &Path) -> Result<SessionFingerprint> {
    scope!("jsonl_extraction", {
        checkpoint!("jsonl_start", path = format!("{}", path.display()));

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
            let line = line.with_context(|| {
                format!(
                    "Failed to read line {} from {}",
                    line_num + 1,
                    path.display()
                )
            })?;

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
                checkpoint!(
                    "jsonl_cwd_found",
                    line = line_num,
                    cwd = format!("{:?}", entry.cwd)
                );
                fingerprint.cwd = entry.cwd;
            }
            if fingerprint.timestamp.is_none() && entry.timestamp.is_some() {
                fingerprint.timestamp = entry.timestamp;
            }
            if fingerprint.session_id.is_none() && entry.session_id.is_some() {
                checkpoint!(
                    "jsonl_session_found",
                    line = line_num,
                    session_id = format!("{:?}", entry.session_id)
                );
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
                        match serde_json::from_value::<AssistantMessage>(msg_value.clone()) {
                            Ok(asst_msg) => {
                                if let Some(content_items) = asst_msg.content {
                                    for item in content_items {
                                        if item.content_type == "tool_use" {
                                            match serde_json::from_value::<ToolUse>(
                                                item.data.clone(),
                                            ) {
                                                Ok(tool_use) => {
                                                    tracing::trace!(
                                                        "extracted tool from JSONL: {}",
                                                        tool_use.name
                                                    );
                                                    tools.push(tool_use.name);
                                                }
                                                Err(e) => tracing::trace!(
                                                    "failed to parse tool_use: {}",
                                                    e
                                                ),
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => tracing::trace!("failed to parse AssistantMessage: {}", e),
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
        fingerprint.recent_prompts = user_prompts.iter().rev().take(3).rev().cloned().collect();

        // Set tool sequence
        fingerprint.tool_sequence = tools;

        checkpoint!(
            "jsonl_complete",
            path = format!("{}", path.display()),
            parsed = lines_parsed,
            skipped = lines_skipped,
            first_prompt = format!("{:?}", fingerprint.first_prompt),
            cwd = format!("{:?}", fingerprint.cwd),
            session_id = format!("{:?}", fingerprint.session_id)
        );

        Ok(fingerprint)
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Matching
// ═══════════════════════════════════════════════════════════════════════════

/// Recognizing a soul across vessels
///
/// Two sets of traces meet: one from the living terminal (scrollback),
/// one from the archived chronicle (JSONL). We compare their marks—
/// do the opening words match? Do they walk the same ground?
/// Do their actions follow the same rhythm?
///
/// The more patterns align, the stronger our certainty:
/// this conversation, once flowing through that terminal,
/// left these exact traces in that log file.
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
///
/// # Automatic Control Flow Tracing
///
/// The `#[vtr::branches]` attribute automatically instruments all match/if branches,
/// emitting `decision!()` calls that show which path was taken through the algorithm.
/// This eliminates manual tracing while providing complete visibility into the decision tree.
#[vtr::branches] // Auto-instruments match/if branches; order vs #[instrument] doesn't matter
#[instrument(ret, skip(scrollback_fp, jsonl_fp), fields(
    scrollback_first = ?scrollback_fp.first_prompt,
    jsonl_first = ?jsonl_fp.first_prompt,
    jsonl_session = ?jsonl_fp.session_id,
))]
pub fn match_fingerprints(
    scrollback_fp: &SessionFingerprint,
    jsonl_fp: &SessionFingerprint,
) -> MatchConfidence {
    // Track confidence evolution as signals accumulate
    // Each matching signal raises confidence from None toward Exact
    // NOTE: #[vtr::branches] auto-traces every match arm, showing the decision path
    let mut score = 0;

    // Check first prompt match (strong signal, worth +2)
    match (&scrollback_fp.first_prompt, &jsonl_fp.first_prompt) {
        (Some(fp1), Some(fp2)) if fp1 == fp2 => score += 2,
        _ => {}
    };

    // Check recent prompts match (any overlap, worth +1)
    for scroll_prompt in &scrollback_fp.recent_prompts {
        if jsonl_fp.recent_prompts.contains(scroll_prompt) {
            score += 1;
            break; // Only count once
        }
    }

    // Check tool sequence similarity (Jaccard > 0.5, worth +1)
    let tool_sim = tool_sequence_similarity(&scrollback_fp.tool_sequence, &jsonl_fp.tool_sequence);
    if tool_sim > 0.5 {
        score += 1;
    }

    // Check CWD match (only if both have cwd, worth +1)
    match (&scrollback_fp.cwd, &jsonl_fp.cwd) {
        (Some(cwd1), Some(cwd2)) if cwd1 == cwd2 => score += 1,
        _ => {}
    };

    // Map score to confidence level
    // Score thresholds: 0=None, 1=Low, 2=Medium, 3=High, 4+=Exact
    match score {
        0 => MatchConfidence::None,
        1 => MatchConfidence::Low,
        2 => MatchConfidence::Medium,
        3 => MatchConfidence::High,
        _ => MatchConfidence::Exact,
    }
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
#[instrument(skip(prompt), fields(prompt_len = prompt.len()))]
fn normalize_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let lowercase = trimmed.to_lowercase();

    // Replace multiple whitespace with single space
    let normalized = lowercase.split_whitespace().collect::<Vec<_>>().join(" ");

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
#[instrument(skip(a, b), fields(a_len = a.len(), b_len = b.len()))]
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
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_prompt() {
        assert_eq!(normalize_prompt("  Fix the BUG  "), "fix the bug");

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
            extract_cwd("cwd: /home/example/workspace\u{a0}\u{e0b4}\u{a0}Model:\u{a0}Opus 4.5"),
            Some(PathBuf::from("/home/example/workspace"))
        );

        // With regular space after path
        assert_eq!(
            extract_cwd("cwd: /home/user/project Model: Claude"),
            Some(PathBuf::from("/home/user/project"))
        );

        // Path with dashes and underscores
        assert_eq!(
            extract_cwd("cwd: /home/example/some-project_dir"),
            Some(PathBuf::from("/home/example/some-project_dir"))
        );

        assert_eq!(extract_cwd("normal text"), None);
    }

    #[test]
    fn test_extract_path_from_start() {
        // Basic path
        assert_eq!(
            extract_path_from_start("/home/user/project"),
            "/home/user/project"
        );

        // Path with trailing space
        assert_eq!(
            extract_path_from_start("/home/user Model: foo"),
            "/home/user"
        );

        // Path with non-breaking space
        assert_eq!(
            extract_path_from_start("/home/user\u{a0}extra"),
            "/home/user"
        );

        // Path with Powerline symbol
        assert_eq!(extract_path_from_start("/path\u{e0b4}status"), "/path");

        // Complex status bar
        assert_eq!(
            extract_path_from_start("/home/example/workspace\u{a0}\u{e0b4}\u{a0}Model:\u{a0}Opus"),
            "/home/example/workspace"
        );
    }

    #[test]
    fn test_extract_from_scrollback() {
        let scrollback = r#"
> implement user authentication
● Bash(ls -la)
● Read(src/auth.rs)
● Edit(src/auth.rs)
cwd: /home/example/workspace
> add tests
● Write(tests/auth_test.rs)
"#;

        let fp = extract_from_scrollback(scrollback);

        assert_eq!(
            fp.first_prompt,
            Some("implement user authentication".to_string())
        );
        assert_eq!(fp.recent_prompts.len(), 2);
        assert!(fp.recent_prompts.contains(&"add tests".to_string()));
        assert_eq!(fp.tool_sequence, vec!["Bash", "Read", "Edit", "Write"]);
        assert_eq!(fp.cwd, Some(PathBuf::from("/home/example/workspace")));
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
