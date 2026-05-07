//! Resume and Continue commands - conversation history browsing
//!
//! - `babel resume` / `babel r` - Interactive TUI pager for session browsing
//! - `babel resume 3 6 9` - Resume sessions by ls-sessions index
//! - `babel continue` / `babel c` - Resume most recent non-running session

use anyhow::{anyhow, Context, Result};
use console::style;
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use tracing::instrument;
use vtr::{boundary, checkpoint};

use babel::core::BabelCore;
use babel::pager::{
    DemoMode, EnrichedSession, ResumeSelection, ResumeSessionSource, RunningStatus,
};

/// Interactive pager for browsing and resuming sessions
///
/// Opens TUI with session list (left) and transcript preview (right).
/// Tab toggles between cwd-only and all projects.
/// Enter launches selected sessions in external terminals without closing the pager.
#[instrument(level = "debug", skip(core))]
pub async fn cmd_resume(core: &mut BabelCore, all: bool, demo: bool, _json: bool) -> Result<()> {
    let demo = demo.then(DemoMode::load_default);
    let mut source = CliResumeSessionSource {
        core,
        demo: demo.clone(),
    };
    let sessions = source.refresh_sessions(false).await?;
    babel::pager::run_resume_pager(&mut source, all, sessions, demo).await
}

/// Resume sessions by their ls-sessions index.
///
/// Resolves indices against the same scan pipeline as `ls-sessions`, then
/// spawns via the backend-generic launch path (kitty or tmux) with cwd
/// fallback and workspace restore.
#[instrument(level = "debug")]
pub async fn cmd_resume_by_index(indices: &[usize]) -> Result<()> {
    let sessions = babel::native_sessions::scan_all(None, &Default::default());

    if sessions.is_empty() {
        return Err(anyhow!("No sessions found"));
    }

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
        let session_key = session.agent_kind.session_key(&session.native_id);
        let selection = ResumeSelection {
            agent_kind: session.agent_kind,
            native_id: session.native_id.clone(),
            session_key,
            project_path: session.project_path.as_deref().map(PathBuf::from),
        };

        match babel::pager::launch_harness_resume(&selection).await {
            Ok(msg) => {
                eprintln!(
                    " {} {} {}",
                    style(format!("{:>2}", idx)).dim(),
                    style(session.agent_kind.slug()).bold(),
                    msg,
                );
            }
            Err(e) => {
                eprintln!(
                    "{} failed to launch {}: {}",
                    style("✗").red(),
                    session.agent_kind.display_name(),
                    e
                );
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
    let enriched: Vec<EnrichedSession> = tokio::task::spawn_blocking(move || {
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
                    created_at: session.created_at,
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
    demo: Option<DemoMode>,
}

#[async_trait::async_trait]
impl ResumeSessionSource for CliResumeSessionSource<'_> {
    async fn refresh_sessions(&mut self, force: bool) -> Result<Vec<EnrichedSession>> {
        if force {
            self.core.refresh().await?;
        }
        let mut sessions = build_resume_sessions(self.core).await?;
        if let Some(demo) = &self.demo {
            demo.anonymize_sessions(&mut sessions);
        }
        Ok(sessions)
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
    babel::pager::launch_harness_resume(selection).await
}

fn get_session_cwd(session_id: &str) -> Result<PathBuf> {
    babel::utility::claude_storage::get_session_cwd(session_id)
}
