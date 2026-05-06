//! Resume and Continue commands - conversation history browsing
//!
//! - `babel resume` / `babel r` - Interactive TUI pager for session browsing
//! - `babel resume 3 6 9` - Resume sessions by ls-sessions index
//! - `babel continue` / `babel c` - Resume most recent non-running session

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use console::style;
use tracing::instrument;
use vtr::{boundary, checkpoint, trace_error};

use babel::core::BabelCore;
use babel::pager::{EnrichedSession, ResumeSelection, ResumeSessionSource, RunningStatus};
use babel::utility::claude_storage::claude_base;

/// Interactive pager for browsing and resuming sessions
///
/// Opens TUI with session list (left) and transcript preview (right).
/// Tab toggles between cwd-only and all projects.
/// Enter launches selected sessions in external terminals without closing the pager.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_resume(core: &mut BabelCore, all: bool, _json: bool) -> Result<()> {
    let mut source = CliResumeSessionSource { core };
    let sessions = source.refresh_sessions(false).await?;
    babel::pager::run_resume_pager(&mut source, all, sessions).await
}

/// Resume sessions by their ls-sessions index.
///
/// Resolves indices against the same scan pipeline as `ls-sessions`, then
/// spawns a kitty terminal per session with the harness-native resume command.
/// Restores to last-known workspace when available in the overlay DB.
#[instrument(level = "debug")]
pub async fn cmd_resume_by_index(indices: &[usize]) -> Result<()> {
    use babel::babel_storage::get_metadata;

    let sessions = babel::native_sessions::scan_all(None, &Default::default());

    if sessions.is_empty() {
        return Err(anyhow!("No sessions found"));
    }

    let conn = babel::babel_storage::init_db().ok();
    let main_socket = babel::kitty::main_socket();

    for &idx in indices {
        if idx == 0 || idx > sessions.len() {
            eprintln!(
                "{} index {} out of range (1-{})",
                style("✗").red(),
                idx,
                sessions.len()
            );
            continue;
        }

        let session = &sessions[idx - 1];
        let spec = session.agent_kind.spec();

        let Some(resume_tpl) = spec.resume_cmd else {
            eprintln!(
                "{} {} has no resume command",
                style("✗").red(),
                session.agent_kind.display_name()
            );
            continue;
        };

        let resume_full = resume_tpl.replace("{}", &session.native_id);
        let parts: Vec<&str> = resume_full.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let cwd = session
            .project_path
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Check overlay DB for workspace placement
        let session_key = session.agent_kind.session_key(&session.native_id);
        let target_workspace = conn
            .as_ref()
            .and_then(|c| get_metadata(c, &session_key).ok().flatten())
            .and_then(|m| m.last_workspace);

        // Spawn in kitty via kitten @ launch
        let mut cmd = Command::new("kitten");
        cmd.arg("@").arg("launch");
        cmd.args(["--type", "os-window"]);
        cmd.args(["--cwd", &cwd.to_string_lossy()]);

        if let Some(ref socket) = main_socket {
            cmd.args(["--to", socket]);
        }

        cmd.arg("--");
        cmd.args(&parts);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let title = session
            .display_name
            .as_deref()
            .map(|t| super::query::sanitize_display(t, 40))
            .unwrap_or_default();

        match cmd.output() {
            Ok(output) if output.status.success() => {
                // Move to workspace if we have placement data
                if let Some(ws) = target_workspace {
                    // Parse new pane id from launch output
                    let pane_id_str = String::from_utf8_lossy(&output.stdout);
                    if let Ok(pane_id) = pane_id_str.trim().parse::<u64>() {
                        if let Ok(Some(pane)) = babel::kitty::get_window(pane_id).await {
                            if let Err(e) = pane.move_to_workspace(ws) {
                                tracing::debug!(error = %e, ws, "Failed to move to workspace");
                            }
                        }
                    }
                }
                eprintln!(
                    " {} {} {}  {}  {}",
                    style(format!("{:>2}", idx)).dim(),
                    style(session.agent_kind.slug()).bold(),
                    style(&cwd.display().to_string()).dim(),
                    title,
                    style(&resume_full).dim(),
                );
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "{} failed to launch {}: {}",
                    style("✗").red(),
                    session.agent_kind.display_name(),
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!("{} failed to spawn kitten: {}", style("✗").red(), e);
            }
        }
    }

    Ok(())
}

async fn build_resume_sessions(core: &BabelCore) -> Result<Vec<EnrichedSession>> {
    // Session scan (heavy file I/O) and pane query (daemon IPC) run concurrently.
    let scan_handle = tokio::task::spawn_blocking(|| {
        let filters = babel::native_sessions::SessionFilters {
            all: true,
            ..Default::default()
        };
        babel::native_sessions::scan_all(None, &filters)
    });

    let mut panes = core.panes().await.unwrap_or_default();
    let sessions = scan_handle.await.context("session scan task failed")?;
    panes.sort_by(|a, b| {
        b.is_focused
            .cmp(&a.is_focused)
            .then_with(|| {
                a.workspace
                    .unwrap_or(i32::MAX)
                    .cmp(&b.workspace.unwrap_or(i32::MAX))
            })
            .then_with(|| a.id().cmp(&b.id()))
    });

    let mut running: HashMap<String, RunningStatus> = HashMap::new();
    for pane in panes {
        let Some(session_id) = pane.session_id.as_deref() else {
            continue;
        };
        let Some(session_key) = pane.agent_kind.normalize_session_claim(session_id) else {
            tracing::warn!(
                pane = %pane.addr.short(),
                detected_harness = %pane.agent_kind,
                stale_session = %session_id,
                "ignoring stale cross-harness resume live session claim"
            );
            continue;
        };
        running
            .entry(session_key)
            .or_insert_with(|| RunningStatus::Active {
                pane_id: pane.id(),
                workspace: pane.workspace,
                focused: pane.is_focused,
                hook_state: pane.hook_state,
                activity_state: pane.activity_state.unwrap_or_default(),
            });
    }

    // Enrichment (SQLite queries + pre-sanitization) off the async executor.
    let mut enriched: Vec<EnrichedSession> = tokio::task::spawn_blocking(move || {
        let conn = babel::babel_storage::init_db().ok();
        let mut enriched: Vec<EnrichedSession> = sessions
            .into_iter()
            .map(|session| {
                let session_key = session.agent_kind.session_key(&session.native_id);
                let meta = conn.as_ref().and_then(|c| {
                    babel::babel_storage::get_metadata(c, &session_key)
                        .ok()
                        .flatten()
                });
                let generated_title = conn.as_ref().and_then(|c| {
                    babel::babel_storage::get_generated_title(c, &session_key)
                        .ok()
                        .flatten()
                });
                EnrichedSession {
                    agent_kind: session.agent_kind,
                    native_id: session.native_id,
                    session_key: session_key.clone(),
                    project_path: session.project_path.map(PathBuf::from),
                    display_name: session.display_name,
                    generated_title,
                    last_prompt: session.last_prompt,
                    turn_count: session.turn_count,
                    last_seen_at: session.last_seen_at,
                    interactive: session.interactive,
                    command_only: session.command_only,
                    has_title: session.has_title,
                    hidden: meta.as_ref().map(|m| m.hidden).unwrap_or(false),
                    custom_icon: meta.as_ref().and_then(|m| m.icon.clone()),
                    unread: !meta.as_ref().map(|m| m.is_read).unwrap_or(true),
                    running_status: running.remove(&session_key).unwrap_or_default(),
                }
            })
            .collect();

        for session in &mut enriched {
            session.pre_sanitize();
        }
        enriched
    })
    .await
    .context("session enrichment task failed")?;

    Ok(enriched)
}

struct CliResumeSessionSource<'a> {
    core: &'a mut BabelCore,
}

#[async_trait::async_trait]
impl ResumeSessionSource for CliResumeSessionSource<'_> {
    async fn refresh_sessions(&mut self, force: bool) -> Result<Vec<EnrichedSession>> {
        if force {
            self.core.refresh().await?;
        }
        build_resume_sessions(self.core).await
    }

    async fn launch_resume(&mut self, selection: &ResumeSelection) -> Result<String> {
        launch_harness_resume(selection).await
    }

    async fn set_hidden(&mut self, session_key: &str, hidden: bool) -> Result<()> {
        let conn = babel::babel_storage::init_db()?;
        babel::babel_storage::set_hidden(&conn, session_key, hidden)
    }

    fn auto_refresh_enabled(&self) -> bool {
        self.core.is_connected()
    }
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

pub(crate) async fn launch_harness_resume(selection: &ResumeSelection) -> Result<String> {
    let spec = selection.agent_kind.spec();
    let resume_cmd = spec.resume_command(&selection.native_id).ok_or_else(|| {
        anyhow!(
            "{} has no resume command",
            selection.agent_kind.display_name()
        )
    })?;
    let parts: Vec<&str> = resume_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!(
            "Empty resume command for {}",
            selection.agent_kind.display_name()
        ));
    }

    let cwd = selection
        .project_path
        .as_ref()
        .filter(|p| p.exists())
        .cloned()
        .or_else(|| {
            if selection.agent_kind == babel::AgentKind::Claude {
                get_session_cwd(&selection.native_id).ok()
            } else {
                None
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    boundary!(
        "harness",
        "resume",
        agent = selection.agent_kind.slug(),
        native_id = selection.native_id.as_str(),
        cwd = format!("{:?}", cwd)
    );

    let (backend, conn) = detect_current_backend()?;

    let launched = backend.launch_pane(&conn, &parts, &cwd).await?;

    // Restore the session to its last-known desktop workspace, if the backend
    // created a distinct desktop window (kitty os-window). Tmux panes share
    // the host window so platform_window_id is None and this is skipped.
    if let Some(pw_id) = launched.platform_window_id {
        let target_workspace = babel::babel_storage::init_db()
            .ok()
            .and_then(|db| {
                babel::babel_storage::get_metadata(&db, &selection.session_key)
                    .ok()
                    .flatten()
            })
            .and_then(|m| m.last_workspace);

        if let Some(ws) = target_workspace {
            if let Err(e) = babel::desktop::move_window_to_workspace(pw_id, ws) {
                tracing::debug!(error = %e, ws, "workspace move failed for resumed session");
            }
        }
    }

    let short_id: String = selection.native_id.chars().take(8).collect();
    Ok(format!(
        "launched {} {}",
        selection.agent_kind.slug(),
        short_id
    ))
}

/// Detect which terminal backend we're running inside and return it with
/// the connection string for this instance.
fn detect_current_backend() -> Result<(std::sync::Arc<dyn babel::backend::TerminalBackend>, String)>
{
    use babel::backend::{kitty::KittyBackend, tmux::TmuxBackend};

    // Kitty: KITTY_LISTEN_ON is set inside kitty shells
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        let backend = std::sync::Arc::new(KittyBackend)
            as std::sync::Arc<dyn babel::backend::TerminalBackend>;
        let conn = babel::kitty::default_socket();
        return Ok((backend, conn));
    }

    // Tmux: $TMUX is set inside tmux sessions
    if let Ok(tmux_val) = std::env::var("TMUX") {
        let backend =
            std::sync::Arc::new(TmuxBackend) as std::sync::Arc<dyn babel::backend::TerminalBackend>;
        if let Some(socket) = tmux_val.splitn(3, ',').next() {
            return Ok((backend, format!("tmux:{socket}")));
        }
    }

    Err(anyhow!(
        "no supported terminal backend detected (need kitty or tmux)"
    ))
}

/// Get working directory for a session by searching ~/.claude/projects/
///
/// Search strategy:
/// 1. Look for session JSONL file in project directories
/// 2. Extract cwd from session metadata if available
/// 3. Fall back to decoding project directory name
/// 4. Last resort: current directory
pub(crate) fn get_session_cwd(session_id: &str) -> Result<PathBuf> {
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
