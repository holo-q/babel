//! Activity State Detection for Claude Code Sessions
//!
//! Detects the current state of a Claude session by analyzing scrollback text patterns.
//! This enables the babel overlay to show accurate visual indicators and make smart
//! decisions about when to allow interaction.
//!
//! State Detection Strategy:
//! - Idle: Prompt ending with ">" awaiting user input
//! - Thinking: Spinner animation or "thinking..." indicators
//! - ToolUse: Tool execution blocks or "Running X..." messages
//! - AwaitingInput: Questions ending with "?" or approval prompts
//! - Unknown: Cannot determine state from available scrollback

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;

use crate::kitty::kitty_socket_path;

/// Current activity state of a Claude Code session
///
/// Serializes to lowercase snake_case for IPC:
/// - `"idle"`, `"thinking"`, `"tool_use"`, `"awaiting_input"`, `"unknown"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Waiting for user input at the prompt
    Idle,
    /// Claude is processing a request (thinking/generating)
    Thinking,
    /// Currently executing a tool/command
    ToolUse,
    /// Waiting for user confirmation or answer to a question
    AwaitingInput,
    /// Cannot determine state
    Unknown,
}

impl SessionState {
    /// Human-readable display name for the state
    pub fn display(&self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Thinking => "thinking",
            SessionState::ToolUse => "tool",
            SessionState::AwaitingInput => "awaiting",
            SessionState::Unknown => "unknown",
        }
    }

    /// Emoji indicator for the state (useful for UI)
    pub fn emoji(&self) -> &'static str {
        match self {
            SessionState::Idle => "⏸",
            SessionState::Thinking => "🧠",
            SessionState::ToolUse => "🔧",
            SessionState::AwaitingInput => "❓",
            SessionState::Unknown => "❔",
        }
    }
}

/// Detect the current state of a Claude session from scrollback text
///
/// Analyzes the last few lines of scrollback to identify patterns that indicate
/// what Claude is currently doing. This is heuristic-based and may not catch all
/// edge cases, but should be reliable for common interaction patterns.
///
/// # Arguments
/// * `scrollback` - Recent scrollback text (typically last 10-20 lines)
///
/// # Detection Patterns
/// - **Idle**: Ends with shell-like prompt (">", "➜", "$")
/// - **Thinking**: Contains spinner chars (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) or "thinking..."
/// - **ToolUse**: Contains tool execution blocks or "Running" messages
/// - **AwaitingInput**: Ends with "?" or contains approval keywords
pub fn detect_state(scrollback: &str) -> SessionState {
    let scrollback = scrollback.trim();
    if scrollback.is_empty() {
        return SessionState::Unknown;
    }

    // Get the last few non-empty lines for analysis
    let lines: Vec<&str> = scrollback
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .collect();

    if lines.is_empty() {
        return SessionState::Unknown;
    }

    let last_line = lines[0];
    let context = lines.join("\n");

    // Check for Idle state - prompt waiting for input
    // Common Claude Code prompts: ">", "➜", or shell-like indicators
    if last_line.trim_end().ends_with('>')
        || last_line.trim_end().ends_with("➜")
        || last_line.trim_end().ends_with('$')
    {
        return SessionState::Idle;
    }

    // Check for Thinking state - processing indicators
    // Spinner characters used by Claude Code's loading animation
    const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    if context.contains("thinking")
        || context.contains("processing")
        || SPINNER_CHARS.iter().any(|c| context.contains(*c))
    {
        return SessionState::Thinking;
    }

    // Check for ToolUse state - tool execution
    // Claude Code shows tool usage with specific formatting
    if context.contains("Running")
        || context.contains("Executing")
        || context.contains("tool:")
        || context.contains("```bash")  // Tool execution blocks
        || context.contains("<function_calls>")  // XML tool invocation
    {
        return SessionState::ToolUse;
    }

    // Check for AwaitingInput state - questions or approval needed
    // Claude often asks for confirmation or clarification
    if last_line.trim_end().ends_with('?')
        || context.contains("(y/n)")
        || context.contains("[Y/n]")
        || context.contains("confirm")
        || context.contains("approve")
        || context.contains("Do you want")
        || context.contains("Should I")
    {
        return SessionState::AwaitingInput;
    }

    // Default to Unknown if no patterns match
    SessionState::Unknown
}

/// Get the last N lines of scrollback from a kitty window
///
/// Uses kitty's remote control protocol to extract scrollback text.
/// This is more reliable than trying to scrape from the terminal directly.
///
/// # Arguments
/// * `kitty_id` - Kitty window ID to query
/// * `lines` - Number of lines to retrieve (typically 10-20 for state detection)
///
/// # Returns
/// The scrollback text, or an error if kitty communication fails
pub fn get_recent_scrollback(kitty_id: u64, lines: usize) -> Result<String> {
    // Use kitty's @ get-text command via kitten
    // Format: kitten @ --to unix:/path/to/sock get-text --match id:WINDOW_ID --extent last_visited_cmd_output
    //
    // For now, we'll use a simpler approach: get screen content
    // In production, you'd want to use the scrollback API properly

    let socket = kitty_socket_path();
    let output = Command::new("kitten")
        .arg("@")
        .arg("--to")
        .arg(&socket)
        .arg("get-text")
        .arg("--match")
        .arg(format!("id:{}", kitty_id))
        .arg("--extent")
        .arg("screen")  // Get visible screen content
        .output()
        .context("Failed to execute kitten @ get-text")?;

    if !output.status.success() {
        anyhow::bail!(
            "kitten @ get-text failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let text = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in scrollback")?;

    // Return the last N lines
    let result: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(result.join("\n"))
}

/// Get the current state of a Claude session by window ID
///
/// Convenience function that combines scrollback retrieval and state detection.
///
/// # Arguments
/// * `kitty_id` - Kitty window ID to check
///
/// # Returns
/// The detected session state, or Unknown on error
pub fn get_window_state(kitty_id: u64) -> SessionState {
    match get_recent_scrollback(kitty_id, 20) {
        Ok(scrollback) => detect_state(&scrollback),
        Err(_) => SessionState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_idle() {
        let scrollback = "Some output\nMore output\n>";
        assert_eq!(detect_state(scrollback), SessionState::Idle);

        let scrollback = "Previous line\n➜";
        assert_eq!(detect_state(scrollback), SessionState::Idle);
    }

    #[test]
    fn test_detect_thinking() {
        let scrollback = "Processing your request...\n⠋ thinking...";
        assert_eq!(detect_state(scrollback), SessionState::Thinking);

        let scrollback = "Claude is thinking about your question";
        assert_eq!(detect_state(scrollback), SessionState::Thinking);
    }

    #[test]
    fn test_detect_tool_use() {
        let scrollback = "Running command: ls -la";
        assert_eq!(detect_state(scrollback), SessionState::ToolUse);

        let scrollback = "Executing tool: file_read";
        assert_eq!(detect_state(scrollback), SessionState::ToolUse);
    }

    #[test]
    fn test_detect_awaiting_input() {
        let scrollback = "Should I proceed with this change?";
        assert_eq!(detect_state(scrollback), SessionState::AwaitingInput);

        let scrollback = "Do you want to continue? (y/n)";
        assert_eq!(detect_state(scrollback), SessionState::AwaitingInput);
    }

    #[test]
    fn test_unknown_state() {
        let scrollback = "";
        assert_eq!(detect_state(scrollback), SessionState::Unknown);

        let scrollback = "Some random output with no clear pattern";
        assert_eq!(detect_state(scrollback), SessionState::Unknown);
    }
}
