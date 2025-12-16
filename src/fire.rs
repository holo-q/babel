//! Fire-and-Forget Claude Session Tracking
//!
//! Tracks background Claude sessions launched via `babel fire`. These are
//! "fire-and-forget" prompts that run detached from any terminal, with
//! optional ambient sound and state persistence for monitoring.
//!
//! State files are stored in `~/.local/state/claude-fire/`:
//! - `{task_id}.pid` - Process ID for liveness checks
//! - `{task_id}.info` - Prompt preview, ambient sound, workdir
//!
//! See [`crate::utility::workdir`] for smart working directory resolution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A tracked fire-and-forget Claude task
///
/// Stored in ~/.local/state/claude-fire/ to allow monitoring and cleanup
/// of background claude sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiredTask {
    /// Unique task identifier (timestamp-based)
    pub task_id: String,
    /// Process ID of the claude instance
    pub pid: u32,
    /// First ~60 chars of the prompt for display
    pub prompt_preview: String,
    /// Working directory where claude was launched
    pub workdir: PathBuf,
    /// Optional ambient sound name being played
    pub ambient_sound: Option<String>,
}

impl FiredTask {
    /// Create a new task ID based on current timestamp
    pub fn new_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("{}-{}", secs, std::process::id())
    }

    /// Check if this task's process is still alive
    pub fn is_alive(&self) -> bool {
        is_process_alive(self.pid)
    }
}

/// Get the state directory for claude-fire task tracking
///
/// Returns: ~/.local/state/claude-fire
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".local/state"))
        .join("claude-fire")
}

/// Track a newly fired task by writing state files
///
/// Creates:
/// - `{task_id}.pid` - Contains the PID
/// - `{task_id}.info` - Contains prompt preview, ambient sound, workdir
pub fn track_task(task: &FiredTask) -> Result<()> {
    let state = state_dir();
    fs::create_dir_all(&state).context("Failed to create state directory")?;

    // Write PID file
    let pid_file = state.join(format!("{}.pid", task.task_id));
    fs::write(&pid_file, task.pid.to_string())
        .context("Failed to write PID file")?;

    // Write info file (prompt + ambient sound + workdir)
    let info_file = state.join(format!("{}.info", task.task_id));
    let info_content = format!(
        "{}\n{}\n{}",
        task.prompt_preview,
        task.ambient_sound.as_deref().unwrap_or(""),
        task.workdir.display()
    );
    fs::write(&info_file, info_content)
        .context("Failed to write info file")?;

    Ok(())
}

/// List all currently running fired tasks
///
/// Checks that PID files reference live processes before returning them.
pub fn list_running_tasks() -> Result<Vec<FiredTask>> {
    let state = state_dir();
    if !state.exists() {
        return Ok(Vec::new());
    }

    let mut tasks = Vec::new();

    for entry in fs::read_dir(&state)? {
        let entry = entry?;
        let path = entry.path();

        // Only process .pid files
        if path.extension().and_then(|s| s.to_str()) != Some("pid") {
            continue;
        }

        let task_id = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Read PID
        let pid_str = fs::read_to_string(&path).context("Failed to read PID file")?;
        let pid: u32 = pid_str.trim().parse().context("Invalid PID in file")?;

        // Check if process is still alive
        if !is_process_alive(pid) {
            continue; // Skip dead processes
        }

        // Read info file
        let info_file = state.join(format!("{}.info", task_id));
        if !info_file.exists() {
            continue;
        }

        let info_content = fs::read_to_string(&info_file)
            .context("Failed to read info file")?;
        let lines: Vec<&str> = info_content.lines().collect();

        let prompt_preview = lines.first().unwrap_or(&"").to_string();
        let ambient_sound = lines.get(1).and_then(|s| {
            if s.is_empty() { None } else { Some(s.to_string()) }
        });
        let workdir = PathBuf::from(lines.get(2).unwrap_or(&""));

        tasks.push(FiredTask {
            task_id,
            pid,
            prompt_preview,
            workdir,
            ambient_sound,
        });
    }

    Ok(tasks)
}

/// Clean up state files for tasks that are no longer running
///
/// Scans the state directory and removes .pid/.info files for dead processes.
pub fn cleanup_finished_tasks() -> Result<usize> {
    let state = state_dir();
    if !state.exists() {
        return Ok(0);
    }

    let mut cleaned = 0;

    for entry in fs::read_dir(&state)? {
        let entry = entry?;
        let path = entry.path();

        // Only process .pid files
        if path.extension().and_then(|s| s.to_str()) != Some("pid") {
            continue;
        }

        // Read PID and check if alive
        if let Ok(pid_str) = fs::read_to_string(&path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if !is_process_alive(pid) {
                    // Remove both .pid and .info files
                    let task_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    let info_file = state.join(format!("{}.info", task_id));

                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(&info_file);
                    cleaned += 1;
                }
            }
        }
    }

    Ok(cleaned)
}

/// Check if a process is still alive
fn is_process_alive(pid: u32) -> bool {
    // On Linux, check /proc/PID/
    let proc_path = format!("/proc/{}", pid);
    if Path::new(&proc_path).exists() {
        return true;
    }

    // Fallback: try kill -0 (send null signal)
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_dir() {
        let dir = state_dir();
        assert!(dir.to_string_lossy().contains("claude-fire"));
    }

    #[test]
    fn test_is_process_alive() {
        // Current process should be alive
        let current_pid = std::process::id();
        assert!(is_process_alive(current_pid));

        // PID 999999 is extremely unlikely to exist
        assert!(!is_process_alive(999999));
    }

    #[test]
    fn test_new_id_format() {
        let id = FiredTask::new_id();
        assert!(id.contains('-'));
        // Should be timestamp-pid format
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 2);
    }
}
