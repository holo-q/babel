//! Resume and Continue commands - conversation history browsing
//!
//! - `babel resume` / `babel r` - Interactive TUI pager for session browsing
//! - `babel continue` / `babel c` - Resume most recent non-running session

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tracing::instrument;
use vtr::{boundary, checkpoint, trace_error};

use babel::core::BabelCore;
use babel::utility::claude_storage::claude_base;

/// Interactive pager for browsing and resuming sessions
///
/// Opens TUI with session list (left) and transcript preview (right).
/// Tab toggles between cwd-only and all projects.
/// Enter resumes selected session.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_resume(core: &BabelCore, all: bool, _json: bool) -> Result<()> {
    // Run the pager (to be implemented in phase 2)
    // For now, just show a placeholder message
    let selected = babel::pager::run_resume_pager(core, all).await?;

    // If a session was selected, launch claude --resume
    if let Some(session_id) = selected {
        launch_claude_resume(&session_id)?;
    }

    Ok(())
}

/// Resume the most recent session not currently running
///
/// Non-interactive: finds the most recent session from history that isn't
/// currently open in any pane, then launches `claude --resume`.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_continue(core: &BabelCore) -> Result<()> {
    // Get running sessions from active windows
    let windows = core.panes().await.unwrap_or_default();
    let running_ids: HashSet<String> = windows
        .iter()
        .filter_map(|w| w.session_id.clone())
        .collect();

    // Get recent sessions from history (check last 50)
    let sessions = core.history(50).await?;

    if sessions.is_empty() {
        return Err(anyhow!("No conversation history found"));
    }

    // Find first non-running session
    let target = sessions
        .iter()
        .find(|s| !running_ids.contains(&s.session_id));

    // Fall back to most recent if all are running (unusual but handle it)
    let target = target
        .or(sessions.first())
        .ok_or_else(|| anyhow!("No sessions available"))?;

    checkpoint!(
        "resume_start",
        session_id = target.session_id.as_str(),
        project = format!("{:?}", target.project)
    );

    // Launch claude --resume, replacing current process
    launch_claude_resume(&target.session_id)
}

/// Launch `claude --resume <session_id>` replacing current process
///
/// Uses exec() to replace the current process with Claude, so this function
/// only returns on error (successful exec never returns).
fn launch_claude_resume(session_id: &str) -> Result<()> {
    // Determine working directory from session
    let cwd = get_session_cwd(session_id)?;

    boundary!(
        "claude",
        "resume",
        session_id = session_id,
        cwd = format!("{:?}", cwd)
    );

    // Use exec to replace current process - never returns on success
    let err = std::process::Command::new("claude")
        .arg("--resume")
        .arg(session_id)
        .current_dir(&cwd)
        .exec();

    // If we get here, exec failed
    Err(anyhow!("Failed to exec claude: {}", err))
}

/// Get working directory for a session by searching ~/.claude/projects/
///
/// Search strategy:
/// 1. Look for session JSONL file in project directories
/// 2. Extract cwd from session metadata if available
/// 3. Fall back to decoding project directory name
/// 4. Last resort: current directory
fn get_session_cwd(session_id: &str) -> Result<PathBuf> {
    let base = claude_base();
    let projects_dir = base.join("projects");

    if !projects_dir.exists() {
        return Err(anyhow!("No projects directory found at {:?}", projects_dir));
    }

    // Search for the session in all project directories
    for entry in std::fs::read_dir(&projects_dir)
        .with_context(|| format!("Failed to read projects directory: {:?}", projects_dir))?
    {
        let entry = entry?;
        let project_dir = entry.path();

        if !project_dir.is_dir() {
            continue;
        }

        // Look for session JSONL file: {session_id}.jsonl
        let session_file = project_dir.join(format!("{}.jsonl", session_id));
        if !session_file.exists() {
            continue;
        }

        // Found the session file - try to extract cwd from session info
        if let Ok(info) = babel::utility::claude_storage::get_session_info(&session_file) {
            if let Some(cwd) = info.cwd {
                if cwd.exists() {
                    return Ok(cwd);
                }
                trace_error!("session_cwd_missing", cwd = format!("{:?}", cwd));
            }
        }

        // Fall back to decoding project directory name
        // Claude encodes paths as: /home/user/project → -home-user-project
        if let Some(project_name) = project_dir.file_name().and_then(|n| n.to_str()) {
            if project_name.starts_with('-') {
                // Decode: "-home-user-project" -> "/home/user/project"
                let decoded = project_name.replacen('-', "/", 1).replace('-', "/");
                let path = PathBuf::from(&decoded);
                if path.exists() {
                    checkpoint!("decoded_cwd", path = format!("{:?}", path));
                    return Ok(path);
                }
            }
        }

        // If we found the session but can't determine cwd, that's still an error
        return Err(anyhow!(
            "Found session {} but could not determine working directory",
            session_id
        ));
    }

    // Session not found in any project
    Err(anyhow!(
        "Session {} not found in ~/.claude/projects/",
        session_id
    ))
}
