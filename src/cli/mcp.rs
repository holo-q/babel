//! MCP Server for Claude session management
//!
//! Exposes babel's Claude session management via the Model Context Protocol.
//! This enables Claude Code (or any MCP client) to query sessions, send prompts,
//! and manage Claude panes programmatically.
//!
//! ## Tools
//!
//! - `claude_sessions`: List all active Claude sessions (fire tasks + terminal windows)
//! - `claude_history`: Query conversation history from ~/.claude
//! - `claude_send`: Send text to a Claude pane
//! - `claude_fire`: Fire a prompt to Claude in background
//! - `claude_focus`: Focus a Claude pane by ID
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
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServiceExt,
};
use serde::Deserialize;
use std::path::PathBuf;

use claude_babel::core::BabelCore;

// ─────────────────────────────────────────────────────────────────────────────
// Request types for MCP tools
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendRequest {
    /// Target window ID
    #[schemars(description = "Kitty window ID to send text to")]
    pub window_id: u64,

    /// Text to send (will be followed by Enter)
    #[schemars(description = "Text to send to the Claude pane (presses Enter after)")]
    pub text: String,

    /// Force send even if there's pending input
    #[schemars(description = "Force send even if there's unsent text in the input area")]
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusRequest {
    /// Window ID to focus
    #[schemars(description = "Kitty window ID to focus")]
    pub window_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FireRequest {
    /// The prompt to send to Claude
    #[schemars(description = "The prompt to fire to a new Claude session")]
    pub prompt: String,

    /// Working directory (uses cwd if omitted)
    #[schemars(description = "Working directory for the Claude session (auto-detected if omitted)")]
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

/// Babel MCP Server - Claude session management tools
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
        tracing::debug!("Initializing BabelMcp tool router");
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// List all active Claude sessions
    ///
    /// Returns both:
    /// - Fire tasks (background Claude sessions)
    /// - Terminal windows (kitty panes running Claude)
    #[tool(description = "List all active Claude sessions - fire tasks and terminal windows with their IDs, titles, states, and workspaces")]
    fn claude_sessions(&self) -> String {
        tracing::info!("Listing Claude sessions");

        // Create ephemeral BabelCore for this query
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                return "Error: No tokio runtime available".to_string();
            }
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;
            core.windows().await
        });

        match result {
            Ok(windows) => {
                if windows.is_empty() {
                    return "No active Claude sessions found".to_string();
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
    #[tool(description = "Query Claude conversation history - returns session IDs, names, project paths, and message summaries")]
    fn claude_history(&self, Parameters(req): Parameters<HistoryRequest>) -> String {
        tracing::info!(limit = ?req.limit, session_ids = ?req.session_ids, "Querying history");

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

    /// Send text to a Claude pane
    ///
    /// Sends text followed by Enter to submit to Claude. Checks for pending
    /// input unless force=true.
    #[tool(description = "Send text to a Claude pane - types the text and presses Enter to submit")]
    fn claude_send(&self, Parameters(req): Parameters<SendRequest>) -> String {
        tracing::info!(window_id = req.window_id, text_len = req.text.len(), force = req.force, "Sending text");

        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return "Error: No tokio runtime available".to_string(),
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;

            // Check for pending input unless force
            if !req.force {
                match core.has_pending_input(req.window_id).await {
                    Ok((true, text)) => {
                        let preview = text.map(|t| {
                            if t.len() > 40 {
                                format!("{}...", &t[..40])
                            } else {
                                t
                            }
                        });
                        return Err(format!(
                            "Window {} has unsent text in input area{}. Use force=true to override.",
                            req.window_id,
                            preview.map(|t| format!(": \"{}\"", t)).unwrap_or_default()
                        ));
                    }
                    Ok((false, _)) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "Failed to check pending input, proceeding");
                    }
                }
            }

            core.send(req.window_id, &req.text).await
                .map_err(|e| format!("Failed to send: {}", e))
        });

        match result {
            Ok(()) => format!("Sent text to window {}", req.window_id),
            Err(e) => e,
        }
    }

    /// Fire a prompt to Claude in a background session
    ///
    /// Launches Claude with the prompt in a new detached terminal.
    /// The working directory is auto-detected or can be explicitly provided.
    #[tool(description = "Fire a prompt to Claude in a new background session - launches a detached terminal")]
    fn claude_fire(&self, Parameters(req): Parameters<FireRequest>) -> String {
        tracing::info!(prompt_len = req.prompt.len(), workdir = ?req.workdir, "Firing prompt");

        // Determine working directory
        let workdir = req.workdir
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

    /// Focus a Claude pane by ID
    ///
    /// Brings the specified window to the foreground and switches to its workspace.
    #[tool(description = "Focus a Claude pane - brings it to foreground and switches to its workspace")]
    fn claude_focus(&self, Parameters(req): Parameters<FocusRequest>) -> String {
        tracing::info!(window_id = req.window_id, "Focusing window");

        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return "Error: No tokio runtime available".to_string(),
        };

        let result = rt.block_on(async {
            let core = BabelCore::connect().await;
            core.focus(req.window_id).await
        });

        match result {
            Ok(()) => format!("Focused window {}", req.window_id),
            Err(e) => format!("Failed to focus window {}: {}", req.window_id, e),
        }
    }
}

#[tool_handler]
impl ServerHandler for BabelMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Babel MCP - Claude session management tools. Use claude_sessions to list \
                 active Claude panes and fire tasks. Use claude_history to query conversation \
                 history. Use claude_send to send prompts to specific windows. Use claude_fire \
                 to start new background Claude sessions. Use claude_focus to bring a window \
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
    tracing::info!("Babel MCP server starting");

    let service = BabelMcp::new().serve(stdio()).await?;

    tracing::info!("MCP server ready, awaiting commands");
    service.waiting().await?;

    tracing::info!("Babel MCP server shutting down");
    Ok(())
}
