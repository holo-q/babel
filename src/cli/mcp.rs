//! MCP Server for agent session management
//!
//! Exposes babel's agent session management via the Model Context Protocol.
//! This enables Claude Code (or any MCP client) to query sessions, send prompts,
//! and manage agent panes programmatically.
//!
//! ## Tools
//!
//! - `claude_sessions`: List all active agent sessions (fire tasks + terminal windows)
//! - `claude_history`: Query conversation history from ~/.claude
//! - `claude_send`: Send text to an agent pane
//! - `claude_fire`: Fire a prompt to an agent in background
//! - `claude_focus`: Focus an agent pane by ID
//!
//! ## Usage
//!
//! ```bash
//! babel mcp  # Runs on stdio transport (JSON-RPC over stdin/stdout)
//! ```
//!
//! Configure in Claude Code's MCP settings to enable these tools.

use anyhow::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use serde::Deserialize;
use std::path::PathBuf;
use vtr::{checkpoint, effect, trace_error};

use claude_babel::core::BabelCore;

// ─────────────────────────────────────────────────────────────────────────────
// Request types for MCP tools
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendRequest {
    /// Target pane ID.
    #[serde(alias = "window_id")]
    #[schemars(description = "Kitty pane ID to send text to")]
    pub pane_id: u64,

    /// Text to send (will be followed by Enter)
    #[schemars(description = "Text to send to the agent pane (presses Enter after)")]
    pub text: String,

    /// Force send even if there's pending input
    #[schemars(description = "Force send even if there's unsent text in the input area")]
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusRequest {
    /// Pane ID to focus.
    #[serde(alias = "window_id")]
    #[schemars(description = "Kitty pane ID to focus")]
    pub pane_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FireRequest {
    /// The prompt to send to the agent
    #[schemars(description = "The prompt to fire to a new agent session")]
    pub prompt: String,

    /// Working directory (uses cwd if omitted)
    #[schemars(description = "Working directory for the agent session (auto-detected if omitted)")]
    pub workdir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HistoryRequest {
    /// Number of recent sessions to return
    #[schemars(description = "Maximum number of sessions to return (default 20)")]
    pub limit: Option<usize>,

    /// Specific session IDs to fetch (overrides limit)
    #[schemars(description = "Specific session IDs to fetch (if provided, ignores limit)")]
    pub session_ids: Option<Vec<String>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP Server Implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Babel MCP Server - agent session management tools
///
/// Runs in ephemeral mode (no daemon required). Each tool call creates
/// a fresh BabelCore instance for the operation.
#[derive(Debug, Clone)]
pub struct BabelMcp {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl BabelMcp {
    pub fn new() -> Self {
        checkpoint!("mcp_init");
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// List all active agent sessions
    ///
    /// Returns both:
    /// - Fire tasks (background agent sessions)
    /// - Terminal windows (kitty panes running an agent)
    #[tool(
        description = "List all active agent sessions - fire tasks and terminal windows with their IDs, titles, states, and workspaces"
    )]
    fn claude_sessions(&self) -> String {
        checkpoint!("mcp_list_sessions");

        // Create ephemeral BabelCore for this query
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                return "Error: No tokio runtime available".to_string();
            }
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;
            core.panes().await
        });

        match result {
            Ok(windows) => {
                if windows.is_empty() {
                    return "No active agent sessions found".to_string();
                }

                // Format windows as JSON for structured consumption
                match serde_json::to_string_pretty(&windows) {
                    Ok(json) => json,
                    Err(e) => format!("Failed to serialize windows: {}", e),
                }
            }
            Err(e) => format!("Failed to list sessions: {}", e),
        }
    }

    /// Query Claude conversation history from ~/.claude
    ///
    /// Returns recent conversations with their session IDs, names, and summaries.
    #[tool(
        description = "Query Claude conversation history - returns session IDs, names, project paths, and message summaries"
    )]
    fn claude_history(&self, Parameters(req): Parameters<HistoryRequest>) -> String {
        checkpoint!(
            "mcp_query_history",
            limit = format!("{:?}", req.limit),
            session_ids = format!("{:?}", req.session_ids)
        );

        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return "Error: No tokio runtime available".to_string(),
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;

            // Get all sessions up to limit (or more if filtering by IDs)
            let limit = if req.session_ids.is_some() {
                usize::MAX
            } else {
                req.limit.unwrap_or(20)
            };

            core.history(limit).await
        });

        match result {
            Ok(mut sessions) => {
                // Filter by session IDs if provided
                if let Some(ref ids) = req.session_ids {
                    sessions.retain(|s| ids.iter().any(|id| s.session_id.contains(id)));
                }

                if sessions.is_empty() {
                    return if req.session_ids.is_some() {
                        "No sessions found matching the provided IDs".to_string()
                    } else {
                        "No conversation history found in ~/.claude".to_string()
                    };
                }

                match serde_json::to_string_pretty(&sessions) {
                    Ok(json) => json,
                    Err(e) => format!("Failed to serialize history: {}", e),
                }
            }
            Err(e) => format!("Failed to query history: {}", e),
        }
    }

    /// Send text to an agent pane
    ///
    /// Sends text followed by Enter to submit to the agent. Checks for pending
    /// input unless force=true.
    #[tool(description = "Send text to an agent pane - types the text and presses Enter to submit")]
    fn claude_send(&self, Parameters(req): Parameters<SendRequest>) -> String {
        effect!(
            "kitty",
            "send_text",
            pane_id = format!("{}", req.pane_id),
            len = format!("{}", req.text.len()),
            force = format!("{}", req.force)
        );

        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return "Error: No tokio runtime available".to_string(),
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;

            // Check for pending input unless force
            if !req.force {
                match core.has_pending_input(req.pane_id).await {
                    Ok((true, text)) => {
                        let preview = text.map(|t| {
                            if t.len() > 40 {
                                format!("{}...", &t[..40])
                            } else {
                                t
                            }
                        });
                        return Err(format!(
                            "Pane {} has unsent text in input area{}. Use force=true to override.",
                            req.pane_id,
                            preview.map(|t| format!(": \"{}\"", t)).unwrap_or_default()
                        ));
                    }
                    Ok((false, _)) => {}
                    Err(e) => {
                        trace_error!("pending_input_check_failed", error = format!("{}", e));
                    }
                }
            }

            core.send(req.pane_id, &req.text)
                .await
                .map_err(|e| format!("Failed to send: {}", e))
        });

        match result {
            Ok(()) => format!("Sent text to pane {}", req.pane_id),
            Err(e) => e,
        }
    }

    /// Fire a prompt to an agent in a background session
    ///
    /// Launches the current provider with the prompt in a new detached terminal.
    /// The working directory is auto-detected or can be explicitly provided.
    #[tool(
        description = "Fire a prompt to an agent in a new background session - launches a detached terminal"
    )]
    fn claude_fire(&self, Parameters(req): Parameters<FireRequest>) -> String {
        effect!(
            "claude",
            "fire_prompt",
            len = format!("{}", req.prompt.len()),
            workdir = format!("{:?}", req.workdir)
        );

        // Determine working directory
        let workdir = req
            .workdir
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

        if !workdir.exists() {
            return format!("Working directory does not exist: {}", workdir.display());
        }

        // Launch claude-fire with the prompt
        // claude-fire handles spawning kitty, running claude, and tracking the task
        let output = std::process::Command::new("claude-fire")
            .arg(&req.prompt)
            .current_dir(&workdir)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                format!("Fired prompt in {}\n{}", workdir.display(), stdout.trim())
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                format!("Fire failed: {}", stderr.trim())
            }
            Err(e) => format!("Failed to execute claude-fire: {}", e),
        }
    }

    /// Focus an agent pane by ID
    ///
    /// Brings the specified pane to the foreground and switches to its workspace.
    #[tool(
        description = "Focus an agent pane - brings it to foreground and switches to its workspace"
    )]
    fn claude_focus(&self, Parameters(req): Parameters<FocusRequest>) -> String {
        effect!("kitty", "focus_pane", pane_id = format!("{}", req.pane_id));

        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return "Error: No tokio runtime available".to_string(),
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;
            core.focus(req.pane_id).await
        });

        match result {
            Ok(()) => format!("Focused pane {}", req.pane_id),
            Err(e) => format!("Failed to focus pane {}: {}", req.pane_id, e),
        }
    }
}

#[tool_handler]
impl ServerHandler for BabelMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Babel MCP - agent session management tools. Use claude_sessions to list \
                 active agent panes and fire tasks. Use claude_history to query conversation \
                 history. Use claude_send to send prompts to specific windows. Use claude_fire \
                 to start new background agent sessions. Use claude_focus to bring a pane \
                 to the foreground."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Run the babel MCP server on stdio transport
pub async fn run_mcp() -> Result<()> {
    checkpoint!("mcp_starting");

    let service = BabelMcp::new().serve(stdio()).await?;

    checkpoint!("mcp_ready");
    service.waiting().await?;

    checkpoint!("mcp_shutdown");
    Ok(())
}
