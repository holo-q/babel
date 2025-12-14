//! Discovery - Match kitty windows to Claude Code sessions
//!
//! This module implements the core discovery logic for identifying which kitty windows
//! are running Claude Code, and matching them to their corresponding conversation sessions
//! in ~/.claude/projects/.
//!
//! ## Matching Strategy
//!
//! 1. **Pre-tagged windows**: Check user_vars for "babel_session_id" - already matched
//! 2. **Title extraction**: Strip "✳ " prefix from active Claude window titles
//! 3. **Conversation search**: Search ~/.claude/projects/ for matching summaries
//! 4. **Tagging**: Once matched, tag the window with session ID for fast future lookups
//! 5. **CWD tiebreaker**: If multiple matches, use working directory proximity
//!
//! ## Window Title Formats
//!
//! - Active sessions: "✳ Summary Text Here"
//! - Idle terminals: "~/path: command"
//! - Some variations include project info
//!
//! The summary text is set by Claude Code itself and appears in the window title.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::claude_storage::{find_session_by_summary, SessionInfo};
use crate::fingerprint::{SessionFingerprint, MatchConfidence};
use crate::kitty::{find_claude_windows, set_user_var, KittyWindow};

/// A kitty window matched to its Claude session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeWindow {
    pub kitty_id: u64,
    pub title: String,
    pub session_id: Option<String>,
    pub session_info: Option<SessionInfo>,
    pub cwd: PathBuf,
    pub is_focused: bool,
    /// Kitty OS window ID (internal)
    pub os_window_id: u64,
    /// X11/Wayland window ID
    pub platform_window_id: u64,
    /// Workspace number (-1 = sticky/all, 0+ = workspace index)
    pub workspace: Option<i32>,
    /// Extracted fingerprint from scrollback (for debugging/verification)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SessionFingerprint>,
    /// Match confidence if matched via fingerprinting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_confidence: Option<MatchConfidence>,
}

/// Fast discovery - just finds Claude windows without expensive session matching
///
/// This is O(1) kitty call + O(1) wmctrl call - no filesystem scanning.
/// For session info, call `enrich_window` on specific windows.
pub fn discover_claude_windows() -> Result<Vec<ClaudeWindow>> {
    use crate::kitty::get_all_workspaces;

    let claude_windows = find_claude_windows().context("Failed to find claude windows")?;

    // Get workspace mappings in one call
    let workspaces = get_all_workspaces();

    let discovered = claude_windows
        .into_iter()
        .map(|window| {
            // Check if already tagged (fast - just reads user_vars from kitty output)
            let session_id = window
                .user_vars
                .get("babel_session_id")
                .filter(|s| !s.is_empty())
                .cloned();

            // Look up workspace for this OS window
            let workspace = workspaces.get(&window.platform_window_id).copied();

            ClaudeWindow {
                kitty_id: window.id,
                title: window.title.clone(),
                session_id,
                session_info: None, // Lazy - only load on demand
                cwd: window.cwd.clone(),
                is_focused: window.is_focused,
                os_window_id: window.os_window_id,
                platform_window_id: window.platform_window_id,
                workspace,
                fingerprint: None, // Only populated in daemon with --details
                match_confidence: None,
            }
        })
        .collect();

    Ok(discovered)
}

/// Enrich a window with full session info (expensive - searches ~/.claude)
///
/// Call this only when you need the full session metadata for a specific window.
/// This does the filesystem scan to match window title → conversation file.
pub fn enrich_window(window: &mut ClaudeWindow) -> Result<()> {
    // If already has session_info, skip
    if window.session_info.is_some() {
        return Ok(());
    }

    // Try to match via user_vars first (already tagged)
    if let Some(ref session_id) = window.session_id {
        if let Some(info) = load_session_by_id(session_id)? {
            window.session_info = Some(info);
            return Ok(());
        }
    }

    // Extract summary from title and search
    let summary = extract_summary_from_title(&window.title);
    if summary.is_empty() {
        return Ok(()); // Not an active Claude session
    }

    if let Some(info) = find_session_by_summary(&summary)? {
        // Tag the window for fast future lookups
        let _ = tag_window(window.kitty_id, &info.session_id);
        window.session_id = Some(info.session_id.clone());
        window.session_info = Some(info);
    }

    Ok(())
}

/// Match a single window to its session
///
/// Matching algorithm:
/// 1. Check user_vars for "babel_session_id" - already tagged
/// 2. If not tagged:
///    - Extract summary from title (strip "✳ " prefix)
///    - Search ~/.claude/projects/ for matching summaries
///    - If found, tag window with session_id for future lookups
/// 3. Return the session ID (as SessionInfo) if matched, None otherwise
pub fn match_window_to_session(window: &KittyWindow) -> Result<Option<SessionInfo>> {
    // Check if already tagged
    if let Some(session_id) = window.user_vars.get("babel_session_id") {
        if !session_id.is_empty() {
            // Try to load SessionInfo from the tagged session_id
            // We'll search by session_id in the conversation files
            if let Some(info) = load_session_by_id(session_id)? {
                return Ok(Some(info));
            }
            // If we can't find the session file anymore, fall through to re-match
        }
    }

    // Extract summary from title
    let summary = extract_summary_from_title(&window.title);
    if summary.is_empty() {
        return Ok(None); // Not an active Claude session
    }

    // Find matching session by searching conversation files
    let session_info = find_session_by_summary(&summary)?;

    // If we found a match, tag the window
    if let Some(ref info) = session_info {
        tag_window(window.id, &info.session_id)?;
    }

    Ok(session_info)
}

/// Tag a window with its session ID for fast future lookups
///
/// Uses kitty's user_vars feature to persistently associate a session ID with a window.
/// The tag survives as long as the window exists.
pub fn tag_window(kitty_id: u64, session_id: &str) -> Result<()> {
    set_user_var(kitty_id, "babel_session_id", session_id)
        .context("Failed to tag window with session ID")
}

/// Find a window by its session ID
///
/// Searches all windows for one tagged with the given session ID.
pub fn find_window_by_session(session_id: &str) -> Result<Option<ClaudeWindow>> {
    let windows = discover_claude_windows()?;
    Ok(windows.into_iter().find(|w| {
        w.session_info
            .as_ref()
            .map(|s| s.session_id.as_str())
            == Some(session_id)
    }))
}

// ============================================================================
// Internal Implementation
// ============================================================================

/// Extract summary text from a window title
///
/// Strips the "✳ " prefix from active Claude sessions.
/// Returns empty string if not an active session.
fn extract_summary_from_title(title: &str) -> String {
    if let Some(summary) = title.strip_prefix("✳ ") {
        summary.trim().to_string()
    } else {
        String::new()
    }
}

/// Load session info by session ID
///
/// Searches all projects for a session file matching the given ID.
/// Returns None if not found.
fn load_session_by_id(session_id: &str) -> Result<Option<SessionInfo>> {
    use crate::claude_storage::{get_session_info, list_projects, list_sessions};

    // Search all projects for this session ID
    for project_dir in list_projects()? {
        for session_path in list_sessions(&project_dir)? {
            // Check if filename matches the session ID
            if let Some(file_stem) = session_path.file_stem().and_then(|s| s.to_str()) {
                if file_stem == session_id {
                    let info = get_session_info(&session_path)?;
                    return Ok(Some(info));
                }
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_summary_from_title() {
        assert_eq!(
            extract_summary_from_title("✳ Pipeline Rewrite Assessment"),
            "Pipeline Rewrite Assessment"
        );
        assert_eq!(extract_summary_from_title("~/path: command"), "");
        assert_eq!(extract_summary_from_title("✳ Summary"), "Summary");
    }
}
