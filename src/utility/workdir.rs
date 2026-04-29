//! Smart Working Directory Resolution
//!
//! Intelligent CWD detection for spawning agent sessions in the right context.
//! Used by fire-and-forget prompts, session spawning, and anywhere we need to
//! infer the user's intended working directory.
//!
//! ## Detection Waterfall
//!
//! 1. Explicit argument (user-provided)
//! 2. Kitty focused window's cwd (via remote control)
//! 3. Focused X window's process cwd (/proc/PID/cwd)
//! 4. Project path from process cmdline (.venv, --project flags)
//! 5. Path from window title (JetBrains `[path]`, VSCode patterns)
//! 6. Fallback: ~/Workspace

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::kitty;

/// Resolve the working directory using intelligent fallbacks
///
/// This implements the smart CWD detection waterfall, preferring explicit
/// input, then terminal context, then X11 window context, then heuristics.
///
/// # Arguments
/// * `explicit` - User-provided directory (if any)
///
/// # Returns
/// The resolved working directory (always returns a valid path)
pub fn resolve(explicit: Option<&str>) -> PathBuf {
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
    fallback_workdir()
}

/// Get the fallback working directory when all detection fails
pub fn fallback_workdir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())).join("Workspace")
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
///
/// Uses our kitty module's socket discovery rather than manual globbing.
/// Note: This uses block_on because this is a sync utility function, but
/// list_panes is async (due to timeout protection on kitten subprocess calls).
fn get_kitty_focused_cwd() -> Option<PathBuf> {
    // Use our existing kitty infrastructure
    // block_on is safe here because this utility is only called from CLI contexts
    // where a runtime is already running
    let rt = tokio::runtime::Handle::try_current().ok()?;
    let panes = rt.block_on(kitty::list_panes()).ok()?;

    panes.into_iter().find(|p| p.is_focused).map(|p| p.cwd)
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
    let cmdline = String::from_utf8_lossy(&cmdline_bytes).replace('\0', " "); // Null-separated args

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
    fn test_fallback_workdir() {
        let fallback = fallback_workdir();
        assert!(fallback.to_string_lossy().contains("Workspace"));
    }

    #[test]
    fn test_get_title_path_jetbrains() {
        let title = "MyProject – [~/Workspace/my-project] – IntelliJ IDEA";
        let path = get_title_path(title);
        // This test will only pass if the path actually exists
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
        // Just ensure it doesn't panic
        let _ = get_title_path(title);
    }
}
