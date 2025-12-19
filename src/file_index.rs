//! File Index - Track which files each Claude session has touched
//!
//! Extracts file paths from scrollback using scrollparse patterns (tool calls
//! like Read, Write, Edit). Enables queries like:
//! - "Which sessions have touched this file?"
//! - "What files has this session modified?"
//!
//! Uses incremental scrollback reading from patched kitty (--from-offset) for
//! efficient 60fps+ polling with minimal data transfer.

use anyhow::{Context, Result};
use regex::Regex;
use std::sync::LazyLock;
use tracing::{debug, trace, warn};

/// A file operation extracted from scrollback
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperation {
    /// The file path that was operated on
    pub path: String,
    /// The operation type (Read, Write, Edit, etc.)
    pub operation: String,
}

/// Extract file operations from scrollback text
///
/// Parses tool calls from Claude Code output and extracts file paths.
/// Handles the following tools:
/// - Read(path) - file was read
/// - Write(path, ...) - file was created/overwritten
/// - Edit(path, ...) - file was modified
/// - Glob(pattern) - files matching pattern (if pattern looks like a path)
///
/// # Arguments
/// * `scrollback` - Raw scrollback text (may be incremental delta)
///
/// # Returns
/// Vector of file operations found in the scrollback
pub fn extract_file_operations(scrollback: &str) -> Vec<FileOperation> {
    // Pattern for Claude Code tool calls: ● ToolName(args)
    // The tool name is capitalized, args are everything in parens
    static TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"●\s*(Read|Write|Edit|Glob|Grep|NotebookEdit)\(([^)]+)\)").unwrap()
    });

    let mut operations = Vec::new();

    for cap in TOOL_CALL_RE.captures_iter(scrollback) {
        let tool_name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let args = cap.get(2).map(|m| m.as_str()).unwrap_or("");

        if let Some(path) = extract_path_from_args(tool_name, args) {
            // Skip non-file paths (like patterns, URLs, etc.)
            if is_valid_file_path(&path) {
                trace!(tool = tool_name, path = %path, "Extracted file operation");
                operations.push(FileOperation {
                    path,
                    operation: tool_name.to_string(),
                });
            }
        }
    }

    operations
}

/// Extract file path from tool arguments
///
/// Different tools have different argument formats:
/// - Read(path) - single argument
/// - Write(path, content) - first argument is path
/// - Edit(path, old_string, new_string) - first argument is path
/// - Glob(pattern) - may or may not be a file path
fn extract_path_from_args(tool: &str, args: &str) -> Option<String> {
    match tool {
        "Read" | "Glob" | "Grep" => {
            // Single argument tools - the whole thing is the path/pattern
            let path = args.trim();
            if path.is_empty() {
                None
            } else {
                Some(clean_path(path))
            }
        }
        "Write" | "Edit" | "NotebookEdit" => {
            // Multi-argument tools - first arg is path
            // Arguments are comma-separated, but paths may contain commas in quotes
            // For simplicity, take everything up to first comma not in quotes
            let path = extract_first_arg(args)?;
            Some(clean_path(&path))
        }
        _ => None,
    }
}

/// Extract the first comma-separated argument, handling quoted strings
fn extract_first_arg(args: &str) -> Option<String> {
    let args = args.trim();
    if args.is_empty() {
        return None;
    }

    // If starts with quote, find matching quote
    if args.starts_with('"') {
        if let Some(end) = args[1..].find('"') {
            return Some(args[1..=end].to_string());
        }
    }

    // Otherwise take up to first comma
    if let Some(comma_pos) = args.find(',') {
        Some(args[..comma_pos].trim().to_string())
    } else {
        Some(args.to_string())
    }
}

/// Clean up a file path (remove quotes, normalize)
fn clean_path(path: &str) -> String {
    let path = path.trim();
    // Remove surrounding quotes if present
    let path = if (path.starts_with('"') && path.ends_with('"'))
        || (path.starts_with('\'') && path.ends_with('\''))
    {
        &path[1..path.len() - 1]
    } else {
        path
    };
    path.to_string()
}

/// Check if a string looks like a valid file path
///
/// Filters out:
/// - Empty strings
/// - Glob patterns with wildcards
/// - URLs
/// - Very short paths (likely not real files)
fn is_valid_file_path(path: &str) -> bool {
    if path.is_empty() || path.len() < 2 {
        return false;
    }

    // Skip glob patterns with wildcards
    if path.contains('*') || path.contains('?') {
        return false;
    }

    // Skip URLs
    if path.starts_with("http://") || path.starts_with("https://") {
        return false;
    }

    // Must look like a path (start with / or ./ or letter)
    path.starts_with('/')
        || path.starts_with("./")
        || path.starts_with("../")
        || path.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false)
}

/// Process incremental scrollback and record file touches
///
/// This is the main entry point for the daemon. It:
/// 1. Uses the stored cursor to read only new scrollback
/// 2. Extracts file operations from the delta
/// 3. Records touches to the database
/// 4. Updates the cursor
///
/// # Arguments
/// * `storage` - Database connection for cursors and touches
/// * `pane_addr` - Unique identifier for this pane
/// * `session_id` - Claude session ID (if known)
/// * `scrollback_result` - Result from kitty @ get-text --from-offset
///
/// # Returns
/// Number of new file operations recorded
pub fn process_incremental_scrollback(
    storage: &crate::babel_storage::BabelStorage,
    pane_addr: &str,
    session_id: Option<&str>,
    text: &str,
    cursor: u64,
    truncated: bool,
) -> Result<usize> {
    // If truncated, log warning - we may have missed operations
    if truncated {
        warn!(
            pane_addr = %pane_addr,
            "Scrollback truncated - may have missed file operations"
        );
    }

    // Extract file operations from the delta
    let operations = extract_file_operations(text);
    let count = operations.len();

    if count > 0 {
        debug!(
            pane_addr = %pane_addr,
            count = count,
            "Recording file operations"
        );
    }

    // Record each operation
    if let Some(sid) = session_id {
        for op in &operations {
            storage
                .record_file_touch(sid, &op.path, &op.operation)
                .with_context(|| format!("Failed to record touch for {}", op.path))?;
        }
    }

    // Update cursor for next poll
    storage
        .set_scrollback_cursor(pane_addr, cursor, session_id)
        .context("Failed to update scrollback cursor")?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_read() {
        let scrollback = r#"
● Read(/home/user/project/src/main.rs)
  ⎿  fn main() {
     println!("Hello");
  }
"#;
        let ops = extract_file_operations(scrollback);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/home/user/project/src/main.rs");
        assert_eq!(ops[0].operation, "Read");
    }

    #[test]
    fn test_extract_write() {
        let scrollback = r#"
● Write(/tmp/test.txt, "Hello world")
  ⎿  File written
"#;
        let ops = extract_file_operations(scrollback);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/tmp/test.txt");
        assert_eq!(ops[0].operation, "Write");
    }

    #[test]
    fn test_extract_edit() {
        let scrollback = r#"
● Edit(/home/user/config.toml, old_value, new_value)
  ⎿  File updated
"#;
        let ops = extract_file_operations(scrollback);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/home/user/config.toml");
        assert_eq!(ops[0].operation, "Edit");
    }

    #[test]
    fn test_skip_glob_patterns() {
        let scrollback = r#"
● Glob(src/**/*.rs)
  ⎿  Found 10 files
"#;
        let ops = extract_file_operations(scrollback);
        assert!(ops.is_empty(), "Should skip glob patterns with wildcards");
    }

    #[test]
    fn test_multiple_operations() {
        let scrollback = r#"
● Read(/etc/passwd)
● Write(/tmp/output.txt, data)
● Edit(/home/user/file.py, old, new)
"#;
        let ops = extract_file_operations(scrollback);
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn test_clean_path() {
        assert_eq!(clean_path("  /path/to/file  "), "/path/to/file");
        assert_eq!(clean_path("\"/path/to/file\""), "/path/to/file");
        assert_eq!(clean_path("'/path/to/file'"), "/path/to/file");
    }

    #[test]
    fn test_is_valid_file_path() {
        assert!(is_valid_file_path("/home/user/file.txt"));
        assert!(is_valid_file_path("./relative/path.rs"));
        assert!(is_valid_file_path("src/main.rs"));
        assert!(!is_valid_file_path("*.rs"));
        assert!(!is_valid_file_path("https://example.com"));
        assert!(!is_valid_file_path(""));
    }
}
