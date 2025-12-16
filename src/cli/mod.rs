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

// ═══════════════════════════════════════════════════════════════════════════════
// Target System - Unified window targeting for all action commands
// ═══════════════════════════════════════════════════════════════════════════════

/// Target specification for window commands
///
/// Supports:
/// - Specific window ID: "42"
/// - All windows: "*"
#[derive(Debug, Clone)]
pub enum Target {
    /// Target a specific window by ID
    Window(u64),
    /// Target all Claude windows
    All,
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s == "*" {
            Ok(Target::All)
        } else {
            s.parse::<u64>()
                .map(Target::Window)
                .map_err(|_| format!("Invalid target '{}': expected window ID or '*'", s))
        }
    }
}

/// Resolve a target to a list of window IDs
///
/// This is a helper that converts Target enum to concrete window IDs.
/// For Target::All, uses BabelCore to discover all Claude windows and returns their IDs.
pub async fn resolve_target(core: &BabelCore, target: &Target) -> anyhow::Result<Vec<u64>> {
    match target {
        Target::Window(id) => Ok(vec![*id]),
        Target::All => {
            let windows = core.windows().await?;
            Ok(windows.iter().map(|w| w.kitty_id).collect())
        }
    }
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
    "ls", "ls-terminals", "ls-panes", "get-window", "get-pane",
    "get-scrollback", "history"
];

/// Mutation commands (state-changing) - rendered underlined in help
const MUTATION_COMMANDS: &[&str] = &[
    "focus", "send", "set-icon", "set-read", "set-title", "mv", "fire"
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
    #[command()]
    Ls {
        /// Show detailed multiline info for each session
        #[arg(short, long)]
        details: bool,
    },

    /// List all kitty terminals (not just Claude)
    ///
    /// By default, shows terminals from the current kitty socket.
    /// Use --all to scan ALL kitty sockets on the system (finds orphaned terminals).
    #[command()]
    LsTerminals {
        /// Scan all kitty sockets (finds orphaned terminals on other instances)
        #[arg(short, long)]
        all: bool,
    },

    /// List all kitty panes with their IDs
    ///
    /// Shows all kitty window panes grouped by OS window. Panes are the
    /// individual terminal views within a kitty window.
    #[command()]
    LsPanes,

    /// Get status of a kitty window
    ///
    /// Shows detailed information about a Claude window including session info,
    /// fingerprint data, and activity state. If no window ID is provided, shows
    /// the currently focused Claude window.
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
    #[command()]
    GetScrollback {
        /// Kitty window ID
        window_id: u64,
        /// Maximum number of lines to retrieve
        #[arg(short, long)]
        lines: Option<usize>,
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

    // ─── Actions (underline = mutation, changes state) ───────────────────────────

    /// Focus a Claude window (rofi picker if no ID given)
    #[command()]
    Focus {
        /// Kitty window ID to focus (omit for interactive rofi picker)
        window_id: Option<u64>,
    },

    /// Send text to Claude window(s)
    ///
    /// Target can be a window ID or "*" for all windows.
    #[command()]
    Send {
        /// Target: window ID or "*" for all
        target: Target,

        /// Text to send
        text: String,
    },

    /// Set a custom icon for window(s)
    ///
    /// Associates a custom emoji or icon with a Claude session. The icon
    /// appears in `babel ls` output and can be used to visually mark
    /// important sessions.
    ///
    /// Target can be a window ID or "*" for all windows.
    #[command()]
    SetIcon {
        /// Target: window ID or "*" for all
        target: Target,

        /// Icon/emoji to display (e.g., "🔥", "⭐", "🚧")
        icon: String,
    },

    /// Mark window(s) as read
    ///
    /// Target can be a window ID or "*" for all windows.
    #[command()]
    SetRead {
        /// Target: window ID or "*" for all
        target: Target,
    },

    // ─── Management ─────────────────────────────────────────────────────────────

    /// Set window title(s) or refresh auto-titles
    ///
    /// With a title argument, sets a custom title for the target window(s).
    /// Without a title argument, auto-determines the title from the session
    /// (equivalent to the old update-titles behavior).
    ///
    /// Target can be a window ID or "*" for all windows.
    ///
    /// Examples:
    ///   babel set-title 42 "My Custom Title"   # Set specific title
    ///   babel set-title 42                     # Auto-title from session
    ///   babel set-title *                      # Auto-title all windows
    #[command()]
    SetTitle {
        /// Target: window ID or "*" for all
        target: Target,

        /// Custom title (omit to auto-determine from session)
        title: Option<String>,
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
    /// Workspace sets capture all Claude windows and their positions across workspaces.
    /// Use `babel wset save` and `babel wset load` to manage layouts.
    #[command()]
    WSet {
        #[command(subcommand)]
        command: WSetCommands,
    },

    /// Start the babel daemon (babeld)
    Daemon {
        /// Enable verbose trace logging
        #[arg(long)]
        trace: bool,
    },

    /// Launch interactive TUI debug console
    ///
    /// Requires daemon to be running. Shows live view of:
    /// - Claude windows and their states
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
}

/// WSet management subcommands
#[derive(Subcommand)]
pub enum WSetCommands {
    /// Save current workspace layout
    ///
    /// Captures all Claude windows and their positions across workspaces.
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
    /// Spawns Claude windows from the saved state.
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
