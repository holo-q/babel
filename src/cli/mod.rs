//! CLI definitions for babel
//!
//! This module contains all CLI structure definitions, argument parsing,
//! and styling configuration. The actual command handlers live in submodules.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use claude_babel::core::BabelCore;

// Re-export submodules
pub mod query;
pub mod action;
pub mod mv;
pub mod fingerprint;
pub mod wset;
pub mod legend;
pub mod mcp;
pub mod hook;
pub mod doctor;
pub mod resume;
pub mod fork;

// ═══════════════════════════════════════════════════════════════════════════════
// Target System - Unified window targeting for all action commands
// ═══════════════════════════════════════════════════════════════════════════════

/// Target specification for window commands
///
/// Supports:
/// - Specific window ID: "42"
/// - All windows: "*"
/// - Current window: "." (uses KITTY_WINDOW_ID env var)
#[derive(Debug, Clone)]
pub enum Target {
    /// Target a specific window by ID
    Window(u64),
    /// Target all Claude panes
    All,
    /// Target the current window (from KITTY_WINDOW_ID env var)
    /// This allows Claude to introspect its own pane
    Current,
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "*" => Ok(Target::All),
            "." => Ok(Target::Current),
            _ => s.parse::<u64>()
                .map(Target::Window)
                .map_err(|_| format!("Invalid target '{}': expected window ID, '*', or '.'", s))
        }
    }
}

/// Resolve a target to a list of window IDs
///
/// This is a helper that converts Target enum to concrete window IDs.
/// - Target::Window(id) → [id]
/// - Target::All → all Claude panes from daemon
/// - Target::Current → current window from KITTY_WINDOW_ID env var
pub async fn resolve_target(core: &BabelCore, target: &Target) -> anyhow::Result<Vec<u64>> {
    match target {
        Target::Window(id) => Ok(vec![*id]),
        Target::All => {
            let windows = core.windows().await?;
            Ok(windows.iter().map(|w| w.id()).collect())
        }
        Target::Current => {
            let (id, _socket) = current_pane_info()?;
            Ok(vec![id])
        }
    }
}

/// Get current pane info from kitty env vars
///
/// Returns (window_id, socket) for the current kitty pane.
/// Kitty sets these env vars in every shell it spawns:
/// - KITTY_WINDOW_ID: The pane's window ID
/// - KITTY_LISTEN_ON: The socket path (e.g., "unix:/run/user/1000/kitty.sock-3497")
///
/// This enables Claude to introspect its own pane with full address precision.
pub fn current_pane_info() -> anyhow::Result<(u64, String)> {
    let id_str = std::env::var("KITTY_WINDOW_ID")
        .map_err(|_| anyhow::anyhow!("KITTY_WINDOW_ID not set (not running in kitty?)"))?;

    let id = id_str.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("Invalid KITTY_WINDOW_ID: {}", id_str))?;

    let socket = std::env::var("KITTY_LISTEN_ON")
        .unwrap_or_else(|_| claude_babel::kitty::default_socket());

    Ok((id, socket))
}

/// Get full PaneAddr for the current pane
///
/// Combines KITTY_WINDOW_ID and KITTY_LISTEN_ON into a PaneAddr for
/// precise addressing. Useful for operations that need socket-specific targeting.
pub fn current_pane_addr() -> anyhow::Result<claude_babel::kitty::PaneAddr> {
    let (id, socket) = current_pane_info()?;
    Ok(claude_babel::kitty::PaneAddr::new(socket, id))
}

// ═══════════════════════════════════════════════════════════════════════════════
// CLI Styling - Spaceship semantic command styling
// ═══════════════════════════════════════════════════════════════════════════════

/// Spaceship CLI style: per-command styling based on semantics
///
/// Commands are styled by their behavior:
/// - Query (read-only): italic - safe to run anytime
/// - Mutation (state-changing): underline - modifies state
/// - Namespace (has subcommands): normal - container only
///
/// This visual language provides instant recognition of command safety.
pub fn spaceship_styles() -> clap::builder::Styles {
    use anstyle::{Style, Effects};

    clap::builder::Styles::styled()
        // Literals: no special style (we style command names individually)
        .literal(Style::new())
        // Placeholders (VALUE, FILE, etc): dim
        .placeholder(Style::new().effects(Effects::DIMMED))
        // Headers: bold
        .header(Style::new().effects(Effects::BOLD))
}

// ANSI escape sequences for command name styling in help output
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const RESET: &str = "\x1b[0m";

/// Query commands (read-only, safe) - rendered italic in help
const QUERY_COMMANDS: &[&str] = &[
    "ls", "ls-terminals", "ls-panes", "ls-sockets", "get-window", "get-pane",
    "get-scrollback", "history", "target", "plan", "resume", "continue", "tail"
];

/// Mutation commands (state-changing) - rendered underlined in help
const MUTATION_COMMANDS: &[&str] = &[
    "focus", "send", "type", "broadcast", "set-icon", "set-read", "set-title", "mv", "fire", "reboot", "fork"
];

/// Style command names in help output based on their semantic category
///
/// - Query commands (read-only): italic
/// - Mutation commands (state-changing): underline
/// - Namespace commands (wset, daemon, help): normal
pub fn style_help_output(help: &str) -> String {
    let mut result = String::new();

    for line in help.lines() {
        let styled_line = style_command_line(line);
        result.push_str(&styled_line);
        result.push('\n');
    }

    result
}

/// Style a single line if it contains a command name
fn style_command_line(line: &str) -> String {
    // Command lines in clap start with "  command_name" (2 spaces, then command)
    if !line.starts_with("  ") {
        return line.to_string();
    }

    let trimmed = line.trim_start();

    // Check for query commands (italic)
    for cmd in QUERY_COMMANDS {
        if trimmed.starts_with(cmd) && trimmed[cmd.len()..].starts_with(' ') {
            return line.replacen(cmd, &format!("{}{}{}", ITALIC, cmd, RESET), 1);
        }
    }

    // Check for mutation commands (underline)
    for cmd in MUTATION_COMMANDS {
        if trimmed.starts_with(cmd) && trimmed[cmd.len()..].starts_with(' ') {
            return line.replacen(cmd, &format!("{}{}{}", UNDERLINE, cmd, RESET), 1);
        }
    }

    line.to_string()
}

// ═══════════════════════════════════════════════════════════════════════════════
// CLI Structure
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Parser)]
#[command(name = "babel")]
#[command(about = "Manage Claude Code sessions across kitty windows", long_about = None)]
#[command(styles = spaceship_styles())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output as JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Logging args (--debug) - enforced by spaceship-std
    #[command(flatten)]
    pub logging: spaceship_std::logging::LoggingArgs,
}

#[derive(Subcommand)]
pub enum Commands {
    // ─── Information Retrieval (italic = query, safe to run) ─────────────────────

    /// List all discovered Claude sessions
    ///
    /// Automatically scans all kitty instances on the system. Windows from
    /// non-current sockets are shown but some operations (focus, send) only
    /// work on the current socket.
    #[command()]
    Ls {
        /// Show detailed multiline info for each session
        #[arg(short, long)]
        details: bool,
    },

    /// List all kitty terminals (not just Claude)
    ///
    /// Scans ALL kitty sockets on the system, showing terminals from all
    /// kitty instances. Useful for finding orphaned terminals.
    #[command()]
    LsTerminals,

    /// List all kitty panes with their IDs
    ///
    /// Shows all kitty window panes grouped by OS window. Panes are the
    /// individual terminal views within a kitty window.
    #[command()]
    LsPanes,

    /// List kitty sockets with status
    ///
    /// Shows all kitty instances (sockets) with their status, pane counts,
    /// and which windows belong to each. Socket-first view for multi-instance
    /// awareness.
    #[command()]
    LsSockets,

    /// Get status of a kitty window
    ///
    /// Shows detailed information about a Claude pane including session info,
    /// fingerprint data, and activity state. If no window ID is provided, shows
    /// the currently focused Claude pane.
    #[command()]
    GetWindow {
        /// Kitty window ID to query (omit for focused window)
        window_id: Option<u64>,
    },

    /// Get status of a panel pane
    ///
    /// Shows information about a richspace-babel panel pane.
    #[command()]
    GetPane {
        /// Pane name to query
        pane_name: Option<String>,
    },

    /// Get scrollback text from a window
    ///
    /// Retrieves the full scrollback buffer from a kitty window. Useful for
    /// debugging or piping to other tools.
    ///
    /// Target can be a window ID or "." for current window.
    #[command()]
    GetScrollback {
        /// Target: window ID or "." for current window
        target: Target,
        /// Maximum number of lines to retrieve
        #[arg(short, long)]
        lines: Option<usize>,
    },

    /// Get the title/name of a Claude session
    ///
    /// Returns the conversation name from Claude's history. This is the name
    /// shown in Claude Code's history panel and can be set via `/rename <name>`.
    ///
    /// Falls back to the session summary if no custom name is set.
    ///
    /// Target can be a window ID or "." for current window.
    #[command()]
    GetTitle {
        /// Target: window ID or "." for current window
        target: Target,
    },

    /// Show conversation history from ~/.claude
    ///
    /// Without arguments, shows recent conversations. Pass session IDs as
    /// positional arguments to show specific sessions.
    #[command( alias = "h")]
    History {
        /// Session IDs to show (if none, shows recent conversations)
        #[arg(value_name = "SESSION")]
        sessions: Vec<String>,

        /// Limit number of results (when no session IDs specified)
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Show all sessions (overrides limit)
        #[arg(long)]
        all: bool,
    },

    /// Point-and-click window selection via slop
    ///
    /// Click any kitty window to get the pane IDs contained within it.
    /// Useful for getting target IDs for send/broadcast commands.
    ///
    /// Uses `slop` for X11 window selection - click the kitty window you want
    /// to target. Returns all pane IDs within that window (multiple if split).
    ///
    /// Examples:
    ///   babel target                    # Click window, get pane IDs
    ///   babel send $(babel target) msg  # Send to clicked window
    #[command(name = "target")]
    Target,

    /// Show the todo/plan list for a Claude session
    ///
    /// Extracts the latest todos from the session's JSONL transcript. The todo
    /// list represents Claude's work plan created via the TodoWrite tool.
    ///
    /// Target can be:
    /// - Window ID: Shows plan for that pane's session
    /// - ".": Shows plan for current pane (from KITTY_WINDOW_ID)
    /// - Session ID: Direct lookup by session UUID
    ///
    /// Examples:
    ///   babel plan .           # Current pane's plan
    ///   babel plan 42          # Plan for window 42
    ///   babel plan abc123...   # Plan by session ID
    #[command()]
    Plan {
        /// Target: window ID, "." for current, or session ID
        target: String,
    },

    /// Browse and resume conversation history
    ///
    /// Opens an interactive TUI pager showing sessions from ~/.claude.
    /// Left panel shows session list, right panel shows transcript preview.
    /// Tab toggles between current directory and all projects.
    /// Enter resumes the selected session.
    ///
    /// Examples:
    ///   babel resume              # Browse sessions in current directory
    ///   babel resume --all        # Browse all sessions
    ///   babel r                   # Shorthand
    #[command(visible_alias = "r")]
    Resume {
        /// Show all projects (not just current directory)
        #[arg(short, long)]
        all: bool,
    },

    /// Continue the most recent non-running session
    ///
    /// Non-interactive: finds the most recent session from history that isn't
    /// currently open in any pane, then launches `claude --resume`.
    /// If all recent sessions are running, uses the most recent one.
    ///
    /// Examples:
    ///   babel continue            # Resume most recent idle session
    ///   babel c                   # Shorthand
    #[command(visible_alias = "c")]
    Continue,

    /// Output recent transcript from a session
    ///
    /// Shows the last N messages from a Claude session's transcript.
    /// Useful for reviewing what happened or piping context to other tools.
    ///
    /// Target can be:
    /// - "." → most recent session in current directory (default)
    /// - Window ID → session from that window
    /// - Session ID → direct session reference
    /// - Path → most recent session in that directory
    ///
    /// Examples:
    ///   babel tail                # Last 20 messages from cwd session
    ///   babel tail . -n 50        # Last 50 messages
    ///   babel tail 42             # From window 42
    ///   babel tail abc123         # From session abc123
    #[command()]
    Tail {
        /// Target: ".", window ID, session ID, or path
        #[arg(default_value = ".")]
        target: String,

        /// Number of messages to show
        #[arg(short = 'n', long, default_value = "20")]
        lines: usize,
    },

    /// Fork from another session with full context injection
    ///
    /// Launches a new Claude session primed with the transcript from the source
    /// session, enabling "2nd degree mode" - meta-cognitive reflection on another
    /// Claude's work.
    ///
    /// The forked session receives:
    /// - Recent transcript from the source session
    /// - Prompt priming for introspection and reflection
    /// - Awareness of `babel tail` for further exploration
    ///
    /// Target can be:
    /// - "." → most recent session in current directory (default)
    /// - Window ID → session from that window
    /// - Session ID → direct session reference
    ///
    /// Examples:
    ///   babel fork                # Fork from cwd session (default)
    ///   babel fork .              # Same as above
    ///   babel fork 42             # Fork from window 42
    ///   babel fork abc123         # Fork from session abc123
    ///   babel fork . -n 50        # Fork with more context
    #[command(visible_alias = "f")]
    Fork {
        /// Target: ".", window ID, or session ID
        #[arg(default_value = ".")]
        target: String,

        /// Number of messages to include in context
        #[arg(short = 'n', long, default_value = "30")]
        lines: usize,
    },

    // ─── Actions (underline = mutation, changes state) ───────────────────────────

    /// Focus a Claude pane (interactive picker if no ID given)
    #[command()]
    Focus {
        /// Kitty window ID to focus (omit for interactive picker)
        window_id: Option<u64>,

        /// Search by scrollback content instead of title
        #[arg(long, short)]
        content: bool,
    },

    /// Send text to Claude pane(s) and press Enter
    ///
    /// Sends text followed by Enter (carriage return) to submit to Claude.
    /// If any targeted window has unsent text in the input area, the operation
    /// is aborted and those windows are listed.
    ///
    /// Target can be a window ID, "*" for all windows, or "." for current window.
    ///
    /// Examples:
    ///   babel send 42 "fix the bug"        # Send to window 42
    ///   babel send '*' "run tests"         # Send to all windows
    ///   babel send . "introspect self"     # Send to current window (Claude self-reference)
    #[command()]
    Send {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,

        /// Text to send (will be followed by Enter)
        text: String,

        /// Force send even if there's pending input in the textbox
        #[arg(long, short)]
        force: bool,
    },

    /// Type text into Claude pane(s) without pressing Enter
    ///
    /// Types text into the input area without submitting. Useful for composing
    /// prompts incrementally or when you want manual control over when to send.
    /// If any targeted window has unsent text, the operation is aborted.
    ///
    /// Target can be a window ID, "*" for all windows, or "." for current window.
    ///
    /// Examples:
    ///   babel type 42 "partial prompt..."   # Type without sending
    ///   babel type . "introspect myself"    # Type in current window (Claude self-reference)
    #[command()]
    Type {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,

        /// Text to type (no Enter at end)
        text: String,

        /// Force type even if there's pending input in the textbox
        #[arg(long, short)]
        force: bool,
    },

    /// Broadcast a prompt to all Claude panes
    ///
    /// Sends the same text to every Claude pane and presses Enter. Equivalent
    /// to `babel send '*' "text"` but more explicit about intent.
    ///
    /// If any window has unsent text in the input area, the broadcast is aborted
    /// and those windows are listed. Use --force to override.
    ///
    /// Examples:
    ///   babel broadcast "run lint"          # Send to all windows
    ///   babel broadcast --force "restart"   # Force even with pending input
    #[command()]
    Broadcast {
        /// Text to broadcast (will be followed by Enter)
        text: String,

        /// Force broadcast even if some windows have pending input
        #[arg(long, short)]
        force: bool,
    },

    /// Set a custom icon for window(s)
    ///
    /// Associates a custom emoji or icon with a Claude session. The icon
    /// appears in `babel ls` output and can be used to visually mark
    /// important sessions.
    ///
    /// Target can be a window ID, "*" for all, or "." for current window.
    #[command()]
    SetIcon {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,

        /// Icon/emoji to display (e.g., "🔥", "⭐", "🚧")
        icon: String,
    },

    /// Mark window(s) as read
    ///
    /// Target can be a window ID, "*" for all, or "." for current window.
    #[command()]
    SetRead {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,
    },

    // ─── Management ─────────────────────────────────────────────────────────────

    /// Set window title(s) or refresh auto-titles
    ///
    /// With a title argument, sets a custom title for the target window(s).
    /// Without a title argument, auto-determines the title from the session
    /// (equivalent to the old update-titles behavior).
    ///
    /// Target can be a window ID, "*" for all, or "." for current window.
    ///
    /// Examples:
    ///   babel set-title 42 "My Custom Title"   # Set specific title
    ///   babel set-title 42                     # Auto-title from session
    ///   babel set-title *                      # Auto-title all windows
    ///   babel set-title . "Working on X"       # Set title for current window
    #[command()]
    SetTitle {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,

        /// Custom title (omit to auto-determine from session)
        title: Option<String>,
    },

    /// Solo a single pane for debugging (isolate one pane, hide others)
    ///
    /// Useful when debugging a specific Claude session - hides all other panes
    /// from `babel ls` and similar outputs to reduce noise. Use `--off` to restore.
    ///
    /// Target can be a window ID or "." for current window.
    ///
    /// Examples:
    ///   babel solo 42       # Solo window 42
    ///   babel solo .        # Solo current window
    ///   babel solo --off    # Restore all panes
    #[command()]
    Solo {
        /// Target: window ID or "." for current (omit when using --off)
        target: Option<Target>,

        /// Disable solo mode (restore all panes)
        #[arg(long)]
        off: bool,
    },

    /// Move a directory while preserving Claude conversation history
    ///
    /// When you move a project directory, Claude's conversation history becomes
    /// orphaned because it stores paths like ~/.claude/projects/-home-user-OldProject/.
    /// This command updates those paths so your conversations follow the directory.
    ///
    /// If Claude terminals are open in the source path:
    /// - Idle terminals are automatically migrated (cd + claude -r)
    /// - Active terminals block the move (use --force to override)
    ///
    /// Usage:
    ///   babel mv ~/OldProject ~/NewProject           # Move + update history
    ///   babel mv --dry ~/OldProject ~/NewProject      # Preview changes only
    ///   babel mv --history-only ~/Old ~/New          # Update history without moving
    ///   babel mv --anxious ~/Old ~/New               # Step-by-step confirmation
    ///   babel mv --force ~/Old ~/New                 # Force move even with active terminals
    #[command()]
    Mv {
        /// Source directory path
        source: PathBuf,

        /// Destination directory path
        dest: PathBuf,

        /// Preview changes without executing (shows what would be modified)
        #[arg(long = "dry", id = "dry_run")]
        dry_run: bool,

        /// Only update Claude history, don't move the actual directory
        /// Use this if you already moved the directory manually
        #[arg(long)]
        history_only: bool,

        /// Step-by-step confirmation mode - confirm each action before executing
        /// Useful for debugging or when you want to be extra careful
        #[arg(long)]
        anxious: bool,

        /// Force move even if active Claude terminals exist (they will break)
        #[arg(long)]
        force: bool,
    },

    /// Fire a prompt to Claude in a detached background session
    ///
    /// Launches Claude with your prompt in a new detached terminal. The working
    /// directory is auto-detected from your current context (focused window, IDE,
    /// terminal, etc.) or can be explicitly provided.
    ///
    /// Fire-and-forget sessions are tracked in ~/.local/state/claude-fire/ for
    /// monitoring and cleanup.
    ///
    /// Examples:
    ///   babel fire "Write tests for auth module"
    ///   babel fire -d ~/myproject "Refactor the API"
    ///   babel fire --ambient rain "Long research task"
    #[command()]
    Fire {
        /// The prompt to send to Claude
        prompt: String,

        /// Working directory (auto-detected if omitted)
        #[arg(short = 'd', long = "dir")]
        workdir: Option<PathBuf>,

        /// Ambient sound name to play during task
        #[arg(long)]
        ambient: Option<String>,
    },

    /// List running fire-and-forget tasks
    #[command()]
    FireLs,

    /// Clean up finished fire tasks
    #[command()]
    FireClean,

    /// Reboot Claude pane(s) - close and reopen with same session
    ///
    /// Useful for applying new kitty forks, Claude Code updates, recovering from
    /// frozen conversations, or pre-testing wset configurations. Preserves:
    /// - Session ID (conversation continues)
    /// - Working directory
    /// - Workspace position
    /// - Window geometry
    ///
    /// Target can be a window ID, "*" for all, or "." for current window.
    ///
    /// Examples:
    ///   babel reboot 42       # Reboot window 42
    ///   babel reboot .        # Reboot current window
    ///   babel reboot '*'      # Reboot ALL Claude panes
    #[command()]
    Reboot {
        /// Target: window ID, "*" for all, or "." for current
        target: Target,
    },

    // ─── Namespace Commands (normal = has subcommands or system) ────────────────

    /// Debug fingerprint linkage between terminals, sessions, and directories
    ///
    /// Traces the connection between a terminal window, its scrollback fingerprint,
    /// and matching session JSONL files. Use this to debug why `babel mv` doesn't
    /// detect a session or why matching fails.
    ///
    /// Input is auto-detected:
    ///   - Pure number (42) → window ID
    ///   - Path-like (., ./foo, /path) → directory
    ///   - Otherwise → session ID
    ///
    /// Examples:
    ///   babel fingerprint 42        # Trace window ID 42
    ///   babel fingerprint .         # Trace current directory
    ///   babel fingerprint abc123    # Trace session abc123
    #[command()]
    Fingerprint {
        /// Window ID, directory path, or session ID (auto-detected)
        #[arg(value_name = "INPUT")]
        input: Option<String>,

        /// Force interpretation as window ID
        #[arg(long, short = 'w')]
        window: bool,

        /// Force interpretation as directory
        #[arg(long, short = 'd')]
        dir: bool,

        /// Force interpretation as session ID
        #[arg(long, short = 's')]
        session: bool,
    },

    /// Manage saved workspace sets (WSet)
    ///
    /// Workspace sets capture all Claude panes and their positions across workspaces.
    /// Use `babel wset save` and `babel wset load` to manage layouts.
    #[command(name = "wset")]
    WSet {
        #[command(subcommand)]
        command: WSetCommands,
    },

    /// Start the babel daemon (babeld)
    Daemon {
        /// Enable verbose trace logging
        #[arg(long)]
        trace: bool,

        /// Disable scrollparse activity detection (hook-only mode)
        ///
        /// When enabled, activity state comes solely from Claude Code hooks,
        /// not from scrollback analysis. Useful for verifying hooks work in isolation.
        #[arg(long)]
        no_scrollparse: bool,
    },

    /// Launch interactive TUI debug console
    ///
    /// Requires daemon to be running. Shows live view of:
    /// - Claude panes and their states
    /// - Fired tasks and their status
    /// - IPC traffic (SEND/RECV/EVNT) for debugging
    ///
    /// Use this for debugging IPC communication and as a reference
    /// implementation for external monitors like richmon.
    #[command()]
    Tui,

    /// Stream daemon events to stdout (CLI monitor)
    ///
    /// Subscribes to daemon events and prints them as JSON lines.
    /// Useful for debugging, piping to other tools, or as a reference
    /// for building external monitors.
    ///
    /// Examples:
    ///   babel monitor                    # All events
    ///   babel monitor --filter state     # Only state change events
    ///   babel monitor | jq '.event'      # Pipe to jq for formatting
    #[command()]
    Monitor {
        /// Filter events by type (e.g., "state", "window", "session")
        /// Can be specified multiple times. Empty = all events.
        #[arg(short, long)]
        filter: Vec<String>,
    },

    /// Run MCP server for Claude Code integration
    ///
    /// Exposes babel's Claude session management via the Model Context Protocol.
    /// This enables Claude Code (or any MCP client) to query sessions, send prompts,
    /// and manage Claude panes programmatically.
    ///
    /// Tools exposed:
    ///   - claude_sessions: List all active Claude sessions
    ///   - claude_history: Query conversation history
    ///   - claude_send: Send text to a Claude pane
    ///   - claude_fire: Fire a prompt in background
    ///   - claude_focus: Focus a Claude pane
    ///
    /// Runs on stdio transport (stdin/stdout JSON-RPC).
    #[command()]
    Mcp,

    /// Claude Code hook handlers—the neural interface
    ///
    /// These commands are called by Claude Code hooks to signal lifecycle events.
    /// They receive session_id directly from Claude Code, bypassing the need for
    /// fingerprint matching or scrollback analysis.
    ///
    /// Hooks are configured in ~/.claude/settings.json and call these handlers:
    ///   - Stop hook → `babel hook stop --session <id>`
    ///   - UserPromptSubmit hook → `babel hook prompt --session <id>`
    #[command(name = "hook")]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },

    /// Check system health and kitty patch status
    ///
    /// Diagnoses the babel ecosystem:
    ///   - Daemon status (running, responsive)
    ///   - Kitty patches (screen geometry, etc.)
    ///   - Watcher integration (babel-watcher.py loaded)
    ///   - Socket connectivity
    ///
    /// Use this to troubleshoot issues with pane sorting, geometry tracking,
    /// or real-time event updates.
    #[command()]
    Doctor,
}

/// Hook handler subcommands—direct signals from Claude Code
///
/// All 8 Claude Code lifecycle hooks are wired here:
/// - SessionStart: Session begins or resumes
/// - UserPromptSubmit: User sends a prompt (existing)
/// - PreToolUse: Before tool execution
/// - PostToolUse: After tool completes
/// - Notification: Permission/idle alerts
/// - SubagentStop: Subagent finishes
/// - PreCompact: Before transcript compression
/// - Stop: Claude finishes responding (existing)
#[derive(Subcommand)]
pub enum HookCommands {
    /// Handle Stop event—worker has finished speaking
    ///
    /// Called when Claude Code finishes responding. Marks the session as unread
    /// and lights the ring (amber border) to signal that the worker awaits attention.
    #[command()]
    Stop {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,

        /// Transcript path from hook payload
        #[arg(long)]
        transcript: Option<String>,
    },

    /// Handle UserPromptSubmit event—the Captain speaks
    ///
    /// Called when the user submits a prompt. Marks the session as read and dims
    /// the ring (restore theme border) to signal the worker's output was acknowledged.
    #[command()]
    Prompt {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle PreToolUse event—tool execution begins
    ///
    /// Called before Claude executes a tool (Bash, Edit, Write, Read, etc.).
    /// Sets state to ToolRunning for finer-grained activity tracking.
    #[command()]
    PreTool {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Tool name (e.g., "Bash", "Edit", "Write")
        #[arg(long)]
        tool: String,

        /// Tool input (truncated for safety)
        #[arg(long)]
        input: Option<String>,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle PostToolUse event—tool execution completed
    ///
    /// Called after a tool finishes. Returns state to Working.
    #[command()]
    PostTool {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Tool name (e.g., "Bash", "Edit", "Write")
        #[arg(long)]
        tool: String,

        /// Tool output (truncated for safety)
        #[arg(long)]
        output: Option<String>,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle Notification event—system alerts
    ///
    /// Called on permission requests, idle warnings, and other notifications.
    /// May flash the ring for permission-required notifications.
    #[command()]
    Notification {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Notification type (e.g., "permission", "idle")
        #[arg(long, name = "type")]
        notif_type: String,

        /// Notification message
        #[arg(long)]
        message: Option<String>,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle SessionStart event—session begins or resumes
    ///
    /// Called when a Claude Code session starts or is resumed.
    /// Logs the session start for telemetry.
    #[command()]
    SessionStart {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Working directory
        #[arg(long)]
        cwd: String,

        /// Whether this is a resumed session
        #[arg(long)]
        resumed: bool,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle SubagentStop event—subagent finished
    ///
    /// Called when a Task tool subagent completes its work.
    #[command()]
    SubagentStop {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Subagent ID that completed
        #[arg(long)]
        subagent_id: String,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Handle PreCompact event—transcript compression imminent
    ///
    /// Called before Claude Code compresses the conversation transcript.
    /// Good time to archive or process the full transcript.
    #[command()]
    PreCompact {
        /// Session ID from Claude Code hook payload
        #[arg(long)]
        session: String,

        /// Path to the transcript file
        #[arg(long)]
        transcript: String,

        /// Kitty window ID (from KITTY_WINDOW_ID env var)
        #[arg(long)]
        kitty_id: Option<u64>,
    },

    /// Install babel hooks into Claude Code settings
    ///
    /// Registers babel's hook handlers in ~/.claude/settings.json so Claude Code
    /// will call them on lifecycle events. This is the neural handshake.
    #[command()]
    Install {
        /// Preview changes without writing (dry run)
        #[arg(long)]
        dry_run: bool,
    },
}

/// WSet management subcommands
#[derive(Subcommand)]
pub enum WSetCommands {
    /// Save current workspace layout
    ///
    /// Captures all Claude panes and their positions across workspaces.
    /// WSet files are stored in ~/.config/claude-babel/wsets/
    #[command(alias = "s")]
    Save {
        /// Name for the WSet (defaults to "default")
        name: Option<String>,

        /// Overwrite existing WSet without confirmation
        #[arg(long)]
        force: bool,
    },

    /// Load a workspace layout
    ///
    /// Spawns Claude panes from the saved state.
    /// Sessions that no longer exist in ~/.claude are skipped.
    #[command(alias = "l")]
    Load {
        /// Name of WSet to load (defaults to "default")
        name: Option<String>,

        /// Show what would happen without executing
        #[arg(long = "dry")]
        dry_run: bool,

        /// Step-by-step confirmation mode
        #[arg(long)]
        anxious: bool,
    },

    /// List all saved WSet files
    #[command(alias = "ls")]
    List,

    /// Rename a WSet
    Rename {
        /// Current name
        old: String,
        /// New name
        new: String,
    },

    /// Delete a WSet
    #[command(alias = "rm")]
    Delete {
        /// Name of WSet to delete
        name: String,
    },

    /// Set or show description for a WSet
    Describe {
        /// WSet name
        name: String,
        /// Description (omit to show current)
        description: Option<String>,
    },
}
