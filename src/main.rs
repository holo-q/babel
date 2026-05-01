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

    // Initialize logging - daemon uses custom init with VtrLayer, others use spaceship-std
    // Daemon's VtrLayer captures 50K events in a ring buffer for debugging parallel operations
    let is_daemon = matches!(cli.command, Commands::Daemon { .. });
    if is_daemon {
        // Custom init with VtrLayer - captures all tracing events to ring buffer
        claude_babel::daemon::init_daemon_logging(&cli.logging);
    } else {
        // Standard spaceship-std init for CLI commands
        // Config: ~/Workspace/logging.toml | Logs: journalctl -t babel -f
        spaceship_std::init_logging!("babel", &cli.logging);
    }

    if cli.logging.debug {
        tracing::debug!("debug logging enabled via --debug flag");
    }

    let is_migration_doctor = match &cli.command {
        Commands::Mv { doctor, .. } => cli.doctor || *doctor,
        _ => false,
    };

    let is_migration = matches!(cli.command, Commands::Mv { .. });

    // Connect to daemon or use ephemeral mode. Migration commands are always
    // local lightweight transactions: the daemon can be stale, busy, or blocked
    // on pane parsing, while mv only needs current pane/cwd structure for risk
    // reporting before it mutates native harness storage.
    let mut core = if is_migration {
        BabelCore::local_lightweight().await
    } else {
        BabelCore::connect().await
    };

    // Print mode indicator to stderr for commands that use BabelCore
    // Skip for daemon/tui/monitor/mcp/hook which have their own connection handling
    let show_mode = !is_migration
        && !is_migration_doctor
        && !matches!(
            cli.command,
            Commands::Daemon { .. }
                | Commands::Tui
                | Commands::Monitor { .. }
                | Commands::Mcp
                | Commands::Hook { .. }
        );
    if show_mode && !cli.json {
        eprintln!("[{}]", core.mode_label());
    }

    // Route to appropriate handler based on subcommand
    match cli.command {
        // ─── Daemon Management ───────────────────────────────────────────────────
        Commands::Daemon {
            trace,
            no_scrollparse,
        } => {
            let enable_scrollparse = !no_scrollparse;
            if trace {
                claude_babel::daemon::run_daemon_traced(enable_scrollparse).await
            } else {
                claude_babel::daemon::run_daemon(enable_scrollparse).await
            }
        }

        // ─── Query Commands (read-only, safe) ────────────────────────────────────
        Commands::Ls { details, all } => cli::query::cmd_ls(&core, cli.json, details, all).await,

        Commands::LsTerminals => cli::query::cmd_ls_terminals(&core, cli.json).await,

        Commands::LsPanes => cli::query::cmd_ls_panes(&core, cli.json).await,

        Commands::LsSockets => cli::query::cmd_ls_sockets(&core, cli.json).await,

        Commands::GetWindow { pane_id } => {
            cli::query::cmd_check_agent_pane(&core, pane_id, cli.json).await
        }

        Commands::GetPane { pane_name } => {
            cli::query::cmd_check_pane(&core, pane_name, cli.json).await
        }

        Commands::History {
            sessions,
            limit,
            all,
        } => cli::query::cmd_history(&core, sessions, limit, all, cli.json).await,

        Commands::Target => cli::action::cmd_target(cli.json).await,

        Commands::Plan { target } => cli::query::cmd_plan(&core, &target, cli.json).await,

        Commands::Resume { all } => cli::resume::cmd_resume(&core, all, cli.json).await,

        Commands::Continue => cli::resume::cmd_continue(&core).await,

        Commands::Tail { target, lines } => {
            cli::fork::cmd_tail(&core, &target, lines, cli.json).await
        }

        Commands::Fork {
            target,
            lines,
            mode,
            hsplit,
            vsplit,
            tab,
        } => {
            let location = if hsplit {
                "hsplit"
            } else if vsplit {
                "vsplit"
            } else if tab {
                "tab"
            } else {
                "os-window"
            };
            cli::fork::cmd_fork(&core, &target, lines, &mode, location).await
        }

        // ─── Action Commands (state-changing) ────────────────────────────────────
        Commands::Focus { pane_id, content } => {
            cli::action::cmd_focus(&core, pane_id, content).await
        }

        Commands::GetScrollback { target, lines } => {
            cli::action::cmd_get_scrollback(&core, &target, lines).await
        }

        Commands::GetTitle { target } => cli::query::cmd_get_title(&core, &target, cli.json).await,

        Commands::Send {
            target,
            text,
            force,
        } => cli::action::cmd_send(&core, &target, &text, force).await,

        Commands::Type {
            target,
            text,
            force,
        } => cli::action::cmd_type(&core, &target, &text, force).await,

        Commands::Broadcast { text, force } => {
            cli::action::cmd_broadcast(&core, &text, force).await
        }

        Commands::SetIcon { target, icon } => {
            cli::action::cmd_set_icon(&core, &target, &icon).await
        }

        Commands::SetRead { target } => cli::action::cmd_set_read(&core, &target).await,

        Commands::SetTitle { target, title } => {
            cli::action::cmd_set_title(&core, &target, title.as_deref()).await
        }

        Commands::Solo { target, off } => cli::action::cmd_solo(&core, target.as_ref(), off).await,

        // ─── Fire-and-Forget Sessions ────────────────────────────────────────────
        Commands::Fire {
            prompt,
            workdir,
            ambient,
        } => cli::action::cmd_fire(&mut core, &prompt, workdir.as_deref(), ambient).await,

        Commands::FireLs => cli::action::cmd_fire_ls(cli.json),

        Commands::FireClean => cli::action::cmd_fire_clean(),

        Commands::Reboot { target } => cli::action::cmd_reboot(&mut core, &target).await,

        // ─── Migration & Diagnostics ─────────────────────────────────────────────
        Commands::Mv {
            source,
            dest,
            doctor,
            dry_run,
            history_only,
            anxious,
            force,
        } => {
            if cli.doctor || doctor {
                cli::doctor::cmd_migration_doctor(&core, source, dest, cli.json).await
            } else {
                cli::mv::cmd_mv(
                    &mut core,
                    source,
                    dest,
                    dry_run,
                    history_only,
                    anxious,
                    force,
                    cli.json,
                    cli.logging.debug,
                )
                .await
            }
        }

        Commands::Fingerprint {
            input,
            window,
            dir,
            session,
        } => cli::fingerprint::cmd_fingerprint(&core, input, window, dir, session, cli.json).await,

        // ─── Workspace Sets ──────────────────────────────────────────────────────
        Commands::WSet { command } => cli::wset::cmd_wset(&core, command, cli.json).await,

        // ─── TUI Debug Console ──────────────────────────────────────────────────
        Commands::Tui => claude_babel::tui::run_tui().await,

        // ─── CLI Event Monitor ──────────────────────────────────────────────────
        Commands::Monitor { filter } => cli::action::cmd_monitor(filter).await,

        // ─── MCP Server ─────────────────────────────────────────────────────────
        Commands::Mcp => cli::mcp::run_mcp().await,

        // ─── Hook Handlers ──────────────────────────────────────────────────────
        // All 8 Claude Code lifecycle hooks wired here
        Commands::Hook { command } => {
            use cli::HookCommands;
            match command {
                HookCommands::Stop {
                    session,
                    kitty_id,
                    transcript,
                } => cli::hook::handle_stop(&session, kitty_id, transcript.as_deref()).await,
                HookCommands::Prompt { session, kitty_id } => {
                    cli::hook::handle_prompt(&session, kitty_id).await
                }
                HookCommands::PreTool {
                    session,
                    tool,
                    input,
                    kitty_id,
                } => cli::hook::handle_pre_tool(&session, kitty_id, &tool, input.as_deref()).await,
                HookCommands::PostTool {
                    session,
                    tool,
                    output,
                    kitty_id,
                } => {
                    cli::hook::handle_post_tool(&session, kitty_id, &tool, output.as_deref()).await
                }
                HookCommands::Notification {
                    session,
                    notif_type,
                    message,
                    kitty_id,
                } => {
                    cli::hook::handle_notification(
                        &session,
                        kitty_id,
                        &notif_type,
                        message.as_deref(),
                    )
                    .await
                }
                HookCommands::SessionStart {
                    session,
                    cwd,
                    resumed,
                    kitty_id,
                } => cli::hook::handle_session_start(&session, kitty_id, &cwd, resumed).await,
                HookCommands::SubagentStop {
                    session,
                    subagent_id,
                    kitty_id,
                } => cli::hook::handle_subagent_stop(&session, kitty_id, &subagent_id).await,
                HookCommands::PreCompact {
                    session,
                    transcript,
                    kitty_id,
                } => cli::hook::handle_pre_compact(&session, kitty_id, &transcript).await,
                HookCommands::Stdin { event, agent } => {
                    let agent_kind: claude_babel::AgentKind = agent.parse().unwrap_or_default();
                    cli::hook::handle_stdin(&event, agent_kind).await
                }
                HookCommands::CodexNotify { payload } => {
                    cli::hook::handle_codex_notify(&payload).await
                }
                HookCommands::Install { dry_run, targets } => {
                    cli::hook::install_hooks(dry_run, &targets).await
                }
            }
        }

        // ─── Diagnostics ──────────────────────────────────────────────────────────
        Commands::Doctor => cli::doctor::cmd_doctor().await,

        // ─── Generation Tools ──────────────────────────────────────────────────────
        Commands::Mmdc {
            dirpath,
            out,
            fire,
            verbose,
        } => cli::mmdc::cmd_mmdc(dirpath, out, fire, verbose).await,
    }
}
