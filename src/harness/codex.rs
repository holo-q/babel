//! Codex-owned protocol surface.
//!
//! Keep Codex storage, roster, and transcript formats behind this namespace so
//! feature code never has to know which Codex file carries which native detail.

use std::path::Path;

pub mod spec;
pub mod title;
pub mod transcript;

/// Build the native argv for resuming a Codex session.
///
/// Codex prompts on startup when the process cwd and stored session cwd differ.
/// Babel launches external terminals as a session launcher, so we pass Codex's
/// own `--cd` flag as well as asking the terminal backend to start in that cwd:
/// the former satisfies Codex's resume contract; the latter keeps shell state
/// intuitive if the user exits the harness.
pub fn resume_command_parts(native_id: &str, cwd: Option<&Path>) -> Vec<String> {
    let mut parts = vec!["codex".to_string(), "resume".to_string()];
    if let Some(cwd) = cwd {
        parts.push("--cd".to_string());
        parts.push(cwd.display().to_string());
    }
    parts.push(native_id.to_string());
    parts
}
