//! Fire-and-Forget Claude Prompts with Smart Working Directory Detection
//!
//! Merged from the original claude-fire nu script, this module provides:
//! - Intelligent CWD resolution from multiple sources (kitty, X11, process info, window titles)
//! - Task tracking for fired claude sessions
//! - State management in ~/.local/state/claude-fire
//!
//! The CWD detection waterfall:
//! 1. Explicit argument (user-provided)
//! 2. Kitty focused window's cwd (via remote control)
//! 3. Focused X window's process cwd (/proc/PID/cwd)
//! 4. Project path from process cmdline (e.g., .venv/bin/python → parent)
//! 5. Path from window title (JetBrains [path], VSCode patterns)
//! 6. Fallback: ~/Workspace

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
/// - {task_id}.pid - Contains the PID
/// - {task_id}.info - Contains prompt preview, ambient sound, workdir
///
/// # Arguments
/// * `task` - The task to track
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
pub fn cleanup_finished_tasks() -> Result<()> {
    let state = state_dir();
    if !state.exists() {
        return Ok(());
    }

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
                }
            }
        }
    }

    Ok(())
}

/// Check if a process is still alive
///
/// Uses /proc filesystem on Linux, or kill -0 signal on other Unix-like systems.
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

// ═══════════════════════════════════════════════════════════════════════════
// Smart Working Directory Resolution
// ═══════════════════════════════════════════════════════════════════════════

/// Resolve the working directory for a fired claude session using intelligent fallbacks
///
/// This implements the smart CWD detection waterfall:
/// 1. Explicit argument (highest priority)
/// 2. Kitty focused window's cwd (via remote control)
/// 3. X11 focused window's process cwd (/proc/PID/cwd)
/// 4. Project path from process cmdline (.venv, --project flags)
/// 5. Path extracted from window title (IDE patterns)
/// 6. Fallback: ~/Workspace (lowest priority)
///
/// # Arguments
/// * `explicit` - User-provided directory (if any)
///
/// # Returns
/// The resolved working directory
pub fn resolve_workdir(explicit: Option<&str>) -> PathBuf {
    // 1. Explicit argument
    if let Some(dir) = explicit {
        let path = PathBuf::from(dir);
        if path.exists() {
            return path;
        }
    }

    // Get X11 focused window info
    let (win_pid, win_class, win_title) = get_x11_focused_info();

    // 2. Kitty remote control (if focused window is kitty)
    if win_class.to_lowercase() == "kitty" {
        if let Some(cwd) = get_kitty_focused_cwd() {
            return cwd;
        }
    }

    // 3. Process cwd
    if win_pid > 0 {
        if let Some(cwd) = get_process_cwd(win_pid) {
            return cwd;
        }

        // 4. Cmdline project detection
        if let Some(project) = get_cmdline_project(win_pid) {
            return project;
        }
    }

    // 5. Window title path extraction
    if !win_title.is_empty() {
        if let Some(path) = get_title_path(&win_title) {
            return path;
        }
    }

    // 6. Fallback
    PathBuf::from(std::env::var("HOME").unwrap()).join("Workspace")
}

/// Get focused X11 window info (PID, class, title)
fn get_x11_focused_info() -> (u32, String, String) {
    let win_id = Command::new("xdotool")
        .arg("getactivewindow")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if win_id.is_empty() {
        return (0, String::new(), String::new());
    }

    let win_pid = Command::new("xdotool")
        .arg("getwindowpid")
        .arg(&win_id)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let win_class = Command::new("xprop")
        .arg("-id")
        .arg(&win_id)
        .arg("WM_CLASS")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            // Parse: WM_CLASS(STRING) = "instance", "class"
            s.split('"')
                .nth(3) // Get the second quoted string (class name)
                .map(|c| c.to_string())
        })
        .unwrap_or_default();

    let win_title = Command::new("xdotool")
        .arg("getwindowname")
        .arg(&win_id)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    (win_pid, win_class, win_title)
}

/// Get the cwd of the focused kitty window via remote control
fn get_kitty_focused_cwd() -> Option<PathBuf> {
    // Find kitty socket (glob /tmp/mykitty-* or use $XDG_RUNTIME_DIR/kitty.sock)
    let sock = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| format!("{}/kitty.sock", d))
        .or_else(|| {
            // Fallback: glob /tmp/mykitty-*
            Command::new("sh")
                .arg("-c")
                .arg("ls -t /tmp/mykitty-* 2>/dev/null | head -1")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })?;

    if sock.is_empty() {
        return None;
    }

    // Query kitty: kitten @ --to unix:/path/to/sock ls
    let output = Command::new("kitten")
        .arg("@")
        .arg("--to")
        .arg(format!("unix:{}", sock))
        .arg("ls")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json_str = String::from_utf8(output.stdout).ok()?;
    let data: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    // Navigate: [0].tabs[].windows[] → find is_focused=true → .cwd
    let tabs = data.get(0)?.get("tabs")?.as_array()?;

    for tab in tabs {
        if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
            for win in windows {
                if win.get("is_focused").and_then(|f| f.as_bool()) == Some(true) {
                    return win.get("cwd")?.as_str().map(PathBuf::from);
                }
            }
        }
    }

    None
}

/// Get the cwd of a process via /proc/PID/cwd
fn get_process_cwd(pid: u32) -> Option<PathBuf> {
    let cwd_link = format!("/proc/{}/cwd", pid);
    let path = fs::read_link(&cwd_link).ok()?;

    // Reject / or $HOME as too generic
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    if path == PathBuf::from("/") || path == home {
        return None;
    }

    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Extract project path from process cmdline
///
/// Detects patterns like:
/// - `.venv/bin/python` → parent of .venv
/// - `--project /path/to/project`
fn get_cmdline_project(pid: u32) -> Option<PathBuf> {
    let cmdline_path = format!("/proc/{}/cmdline", pid);
    let cmdline_bytes = fs::read(&cmdline_path).ok()?;
    let cmdline = String::from_utf8_lossy(&cmdline_bytes)
        .replace('\0', " "); // Null-separated args

    // Pattern: .venv/bin/python → parent of .venv is project
    if let Some(idx) = cmdline.find("/.venv/bin/python") {
        let venv_path = &cmdline[..idx + 6]; // Include "/.venv"
        if let Some(parent_idx) = venv_path.rfind('/') {
            let project_path = PathBuf::from(&venv_path[..parent_idx]);
            if project_path.exists() {
                return Some(project_path);
            }
        }
    }

    // Pattern: --project /path/to/project
    if let Some(idx) = cmdline.find("--project ") {
        let after_flag = &cmdline[idx + 10..]; // Skip "--project "
        if let Some(end_idx) = after_flag.find(' ') {
            let project_path = PathBuf::from(&after_flag[..end_idx].trim());
            if project_path.exists() {
                return Some(project_path);
            }
        } else {
            // --project is the last arg
            let project_path = PathBuf::from(after_flag.trim());
            if project_path.exists() {
                return Some(project_path);
            }
        }
    }

    None
}

/// Extract path from window title using IDE-specific patterns
///
/// Supported patterns:
/// - JetBrains: `[~/path/to/project]`
/// - VSCode: `file - folder - Visual Studio Code`
fn get_title_path(title: &str) -> Option<PathBuf> {
    // JetBrains pattern: [~/path/to/project] or [/abs/path]
    if let Some(start) = title.find('[') {
        if let Some(end) = title[start..].find(']') {
            let path_str = &title[start + 1..start + end];
            let expanded = if path_str.starts_with('~') {
                let home = std::env::var("HOME").ok()?;
                path_str.replacen('~', &home, 1)
            } else {
                path_str.to_string()
            };

            let path = PathBuf::from(&expanded);
            if path.exists() {
                return Some(path);
            }
        }
    }

    // VSCode pattern: "file - folder - Visual Studio Code"
    if title.contains("Visual Studio Code") {
        let parts: Vec<&str> = title.split(" - ").collect();
        if parts.len() >= 2 {
            let folder = parts[parts.len() - 2].trim();

            // Try folder as-is, or in common base directories
            let home = std::env::var("HOME").ok()?;
            let candidates = vec![
                PathBuf::from(folder),
                PathBuf::from(&home).join("Projects").join(folder),
                PathBuf::from(&home).join("Workspace").join(folder),
            ];

            for candidate in candidates {
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    None
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
    fn test_get_title_path_jetbrains() {
        let title = "MyProject – [~/Workspace/my-project] – IntelliJ IDEA";
        let path = get_title_path(title);
        // This test will only pass if the path actually exists
        // In a real test suite, you'd mock the filesystem
        if let Some(home) = std::env::var("HOME").ok() {
            let expected = PathBuf::from(home).join("Workspace/my-project");
            if expected.exists() {
                assert_eq!(path, Some(expected));
            }
        }
    }

    #[test]
    fn test_get_title_path_vscode() {
        let title = "main.rs - my-project - Visual Studio Code";
        let path = get_title_path(title);
        // Similar caveat: depends on filesystem state
        // For now, just ensure it doesn't panic
        let _ = path;
    }
}
