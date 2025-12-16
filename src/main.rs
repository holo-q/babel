//! Babel - CLI for managing Claude Code sessions across kitty windows
//!
//! Two modes of operation:
//! - With daemon: Instant responses via IPC (recommended)
//! - Without daemon: Direct kitty/file queries (fallback)
//!
//! Start daemon: `babel daemon`
//! All other commands auto-detect and use daemon if available.

mod cli;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use claude_babel::core::BabelCore;
use cli::{Cli, Commands};

/// Main entry point - thin dispatcher to CLI handlers
///
/// This main.rs is intentionally kept minimal (~150 lines). All command logic
/// lives in the cli/ module tree:
///   - cli/query.rs      → Read-only commands (ls, get-window, history, etc.)
///   - cli/action.rs     → Mutation commands (focus, send, set-icon, etc.)
///   - cli/mv.rs         → Directory migration (babel mv)
///   - cli/fingerprint.rs → Debug fingerprinting (babel fingerprint)
///   - cli/wset.rs       → Workspace sets (babel wset)
///
/// The dispatcher:
///   1. Handles --help with semantic styling (italic=query, underline=mutation)
///   2. Initializes logging via spaceship-std
///   3. Routes to appropriate handler based on subcommand
#[tokio::main]
async fn main() -> Result<()> {
    // Intercept help display to apply semantic command styling
    // Cases: `babel --help`, `babel -h`, or `babel` with no subcommand
    let args: Vec<String> = std::env::args().collect();
    let wants_help = args.iter().any(|a| a == "--help" || a == "-h");
    let no_subcommand = args.len() == 1; // Just "babel" with no args

    if (wants_help && args.len() <= 2) || no_subcommand {
        // Show styled help - print directly, escape sequences included
        let help = Cli::command().render_help().to_string();
        let styled = cli::style_help_output(&help);
        print!("{}", styled);

        // Exit with error code if no subcommand (clap would too)
        if no_subcommand && !wants_help {
            std::process::exit(2);
        }
        return Ok(());
    }

    // Parse CLI first to get --debug flag before logging init
    let cli = Cli::parse();

    // Initialize logging via spaceship-std (centralized config + SIGHUP hot-reload)
    // Config: ~/Workspace/logging.toml | Logs: journalctl -t babel -f
    // "babel" = config key and journald identifier, "claude_babel" = Rust crate for filtering
    // --debug flag forces debug level regardless of config
    spaceship_std::logging::init("babel", "claude_babel", &cli.logging);

    if cli.logging.debug {
        tracing::debug!("debug logging enabled via --debug flag");
    }

    // Connect to daemon or use ephemeral mode
    let mut core = BabelCore::connect().await;

    // Print mode indicator to stderr for commands that use BabelCore
    // Skip for daemon/tui/monitor which have their own connection handling
    let show_mode = !matches!(
        cli.command,
        Commands::Daemon { .. } | Commands::Tui | Commands::Monitor { .. }
    );
    if show_mode && !cli.json {
        eprintln!("[{}]", core.mode_label());
    }

    // Route to appropriate handler based on subcommand
    match cli.command {
        // ─── Daemon Management ───────────────────────────────────────────────────
        Commands::Daemon { trace } => {
            if trace {
                claude_babel::daemon::run_daemon_traced().await
            } else {
                claude_babel::daemon::run_daemon().await
            }
        }

        // ─── Query Commands (read-only, safe) ────────────────────────────────────
        Commands::Ls { details, all } => {
            cli::query::cmd_ls(&core, cli.json, details, all).await
        }

        Commands::LsTerminals => {
            cli::query::cmd_ls_terminals(&core, cli.json).await
        }

        Commands::LsPanes => {
            cli::query::cmd_ls_panes(&core, cli.json).await
        }

        Commands::GetWindow { window_id } => {
            cli::query::cmd_check_window(&core, window_id, cli.json).await
        }

        Commands::GetPane { pane_name } => {
            cli::query::cmd_check_pane(&core, pane_name, cli.json).await
        }

        Commands::History { sessions, limit, all } => {
            cli::query::cmd_history(&core, sessions, limit, all, cli.json).await
        }

        // ─── Action Commands (state-changing) ────────────────────────────────────
        Commands::Focus { window_id } => {
            cli::action::cmd_focus(&core, window_id).await
        }

        Commands::GetScrollback { window_id, lines } => {
            cli::action::cmd_get_scrollback(&core, window_id, lines).await
        }

        Commands::Send { target, text } => {
            cli::action::cmd_send(&core, &target, &text).await
        }

        Commands::SetIcon { target, icon } => {
            cli::action::cmd_set_icon(&core, &target, &icon).await
        }

        Commands::SetRead { target } => {
            cli::action::cmd_set_read(&core, &target).await
        }

        Commands::SetTitle { target, title } => {
            cli::action::cmd_set_title(&core, &target, title.as_deref()).await
        }

        // ─── Fire-and-Forget Sessions ────────────────────────────────────────────
        Commands::Fire { prompt, workdir, ambient } => {
            cli::action::cmd_fire(&mut core, &prompt, workdir.as_deref(), ambient).await
        }

        Commands::FireLs => {
            cli::action::cmd_fire_ls(cli.json)
        }

        Commands::FireClean => {
            cli::action::cmd_fire_clean()
        }

        // ─── Migration & Diagnostics ─────────────────────────────────────────────
        Commands::Mv { source, dest, dry_run, history_only, anxious, force } => {
            cli::mv::cmd_mv(&mut core, source, dest, dry_run, history_only, anxious, force, cli.json).await
        }

        Commands::Fingerprint { input, window, dir, session } => {
            cli::fingerprint::cmd_fingerprint(&core, input, window, dir, session, cli.json).await
        }

        // ─── Workspace Sets ──────────────────────────────────────────────────────
        Commands::WSet { command } => {
            cli::wset::cmd_wset(&core, command, cli.json).await
        }

        // ─── TUI Debug Console ──────────────────────────────────────────────────
        Commands::Tui => {
            claude_babel::tui::run_tui().await
        }

        // ─── CLI Event Monitor ──────────────────────────────────────────────────
        Commands::Monitor { filter } => {
            cli::action::cmd_monitor(filter).await
        }
    }
}
