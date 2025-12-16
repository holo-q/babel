//! Query commands - read-only operations that fetch data about Claude sessions
//!
//! These commands never modify state and are safe to run anytime.
//! All commands support both daemon and direct mode, falling back automatically.

use std::collections::HashMap;

use anyhow::{Context, Result};
use console::{style, Style};

use claude_babel::utility::claude_storage::SessionInfo;
use claude_babel::core::BabelCore;
use claude_babel::utility::claude_discovery::{detect_claude_signals, ClaudeWindow};
use claude_babel::fingerprint::extract_from_scrollback;
use claude_babel::kitty::{discover_all_instances, get_scrollback, list_panes};
use claude_babel::babel_storage::{get_metadata, init_db};
use claude_babel::ActivityState;

// ═══════════════════════════════════════════════════════════════════════════════
// Core Query Commands
// ═══════════════════════════════════════════════════════════════════════════════

/// List all Claude sessions (windows with claude running)
///
/// When `all` is true, scans all kitty sockets on the system instead of just the current one.
/// This is useful when you have multiple kitty instances running.
pub async fn cmd_ls(core: &BabelCore, json: bool, details: bool, all: bool) -> Result<()> {
	use claude_babel::kitty::{list_all_panes, get_all_workspaces, PaneAddr};
	use claude_babel::utility::claude_discovery::{detect_claude_signals, ClaudeWindow};

	let mut windows = if all {
		// Scan all kitty sockets for Claude windows
		let all_panes = list_all_panes().context("Failed to list all panes")?;
		let workspaces = get_all_workspaces();

		// Filter to Claude windows and convert to ClaudeWindow
		all_panes
			.into_iter()
			.filter(|p| detect_claude_signals(p).is_claude())
			.map(|p| {
				let ws = workspaces.get(&p.platform_window_id).copied();
				ClaudeWindow {
					addr: PaneAddr::new(p.socket.clone(), p.id),
					title: p.title.clone(),
					session_id: None,
					session_info: None,
					cwd: p.cwd.clone(),
					is_focused: p.is_focused,
					os_window_id: p.os_window_id,
					platform_window_id: p.platform_window_id,
					workspace: ws,
					activity_state: None, // Would need scrollback to determine
					fingerprint: None,
					match_confidence: None,
				}
			})
			.collect()
	} else if details {
		get_windows_with_fingerprints(core).await?
	} else {
		core.windows().await?
	};

	if json {
		println!("{}", serde_json::to_string_pretty(&windows)?);
		return Ok(());
	}

	if windows.is_empty() {
		println!("No Claude sessions found");
		return Ok(());
	}

	// Sort by workspace, then by OS window ID for consistent grouping
	windows.sort_by(|a, b| {
		let ws_a = a.workspace.unwrap_or(999);
		let ws_b = b.workspace.unwrap_or(999);
		ws_a.cmp(&ws_b)
		    .then(a.os_window_id.cmp(&b.os_window_id))
		    .then(a.id().cmp(&b.id()))
	});

	// Group and display by workspace
	let mut current_workspace: Option<i32> = None;
	let dim = Style::new().dim();

	for win in &windows {
		let ws = win.workspace.unwrap_or(-2); // -2 = unknown

		// Print workspace header when workspace changes
		if current_workspace != Some(ws) {
			if current_workspace.is_some() {
				println!(); // Blank line between workspaces
			}
			let ws_name = match ws {
				-1 => "Sticky".to_string(),
				-2 => "?".to_string(),
				n => format!("{}", n + 1), // 0-indexed to 1-indexed
			};
			println!("{}", dim.apply_to(format!("─── Workspace {} ───", ws_name)));
			current_workspace = Some(ws);
		}

		if details {
			print_window_detailed(win)?;
		} else {
			print_window(win)?;
		}
	}

	Ok(())
}

/// List all kitty terminals (not just Claude sessions)
///
/// Scans ALL kitty sockets on the system, showing terminals from all instances.
pub async fn cmd_ls_terminals(_core: &BabelCore, json: bool) -> Result<()> {
	let instances = discover_all_instances();

	if json {
		println!("{}", serde_json::to_string_pretty(&instances)?);
		return Ok(());
	}

	if instances.is_empty() {
		println!("No kitty sockets found");
		return Ok(());
	}

	let total_windows: usize = instances.iter().map(|i| i.panes.len()).sum();
	let responsive: usize = instances.iter().filter(|i| i.is_responsive).count();

	println!("Kitty instances ({} sockets, {} responsive, {} total windows):", instances.len(), responsive, total_windows);
	println!();

	for instance in &instances {
		let status = if instance.is_current {
			"● current"
		} else if instance.is_responsive {
			"○ other"
		} else {
			"✗ dead"
		};

		let pid_str = instance.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string());

		println!("  {} [PID {}] {} windows", status, pid_str, instance.panes.len());

		if let Some(ref err) = instance.error {
			println!("    Error: {}", err);
			continue;
		}

		// Show windows for this instance
		for win in &instance.panes {
			let signals = detect_claude_signals(win);
			let cmdline = win.foreground_processes
			                 .first()
			                 .and_then(|p| p.cmdline.first())
			                 .map(|s| s.rsplit('/').next().unwrap_or(s))
			                 .unwrap_or("?");

			let title: String = win.title.chars().take(40).collect();
			let title = if win.title.len() > 40 { format!("{}…", title) } else { title };

			println!("    {:>5} {} {:8}  {}", win.id, signals.indicator(), cmdline, title);
		}
		println!();
	}

	if instances.len() > 1 {
		println!("⚠ Multiple kitty instances detected - terminals may be unreachable!");
		println!("  Consider: pkill kitty && kitty (to consolidate)");
	}

	Ok(())
}

/// List all kitty panes grouped by OS window
pub async fn cmd_ls_panes(_core: &BabelCore, json: bool) -> Result<()> {
	let windows = list_panes().context("Failed to list kitty panes")?;

	if json {
		println!("{}", serde_json::to_string_pretty(&windows)?);
		return Ok(());
	}

	if windows.is_empty() {
		println!("No kitty panes found");
		return Ok(());
	}

	// Group by OS window
	let mut by_os_window: HashMap<u64, Vec<_>> = HashMap::new();
	for win in windows {
		by_os_window.entry(win.os_window_id).or_default().push(win);
	}

	let total_panes: usize = by_os_window.values().map(|v| v.len()).sum();
	println!("Kitty panes ({} panes in {} OS windows):", total_panes, by_os_window.len());
	println!();

	for (os_id, panes) in by_os_window.iter() {
		println!("  OS Window {} ({} panes):", os_id, panes.len());
		for win in panes {
			let title: String = win.title.chars().take(50).collect();
			let title = if win.title.len() > 50 {
				format!("{}…", title)
			} else {
				title
			};
			let focus = if win.is_focused { "*" } else { " " };
			println!("    {:>5}{} {}", win.id, focus, title);
		}
	}

	Ok(())
}

/// Check status of a specific window or the focused window
pub async fn cmd_check_window(core: &BabelCore, window_id: Option<u64>, json: bool) -> Result<()> {
	let window = core.window(window_id).await?;

	match window {
		Some(win) => {
			if json {
				println!("{}", serde_json::to_string_pretty(&win)?);
			} else {
				print_window(&win)?;
			}
		}
		None => {
			if window_id.is_some() {
				println!("Window not found or not a Claude session");
				println!();
				show_available_windows(core).await?;
			} else {
				println!("No focused Claude window found");
			}
		}
	}

	Ok(())
}

/// Check status of a specific richspace pane
pub async fn cmd_check_pane(_core: &BabelCore, pane_name: Option<String>, _json: bool) -> Result<()> {
	// TODO: Implement once richspace-babel pane querying is available
	match pane_name {
		Some(name) => {
			println!("Pane '{}' not found (richspace-babel integration pending)", name);
		}
		None => {
			println!("No pane name specified");
			println!();
			println!("Usage: babel check-pane <PANE_NAME>");
		}
	}
	Ok(())
}

/// Show conversation history
pub async fn cmd_history(core: &BabelCore, sessions: Vec<String>, limit: usize, all: bool, json: bool) -> Result<()> {
	// Determine effective limit
	let effective_limit = if all { usize::MAX } else { limit };

	// If specific sessions requested, filter to those
	let results = if !sessions.is_empty() {
		// TODO: Implement session ID filtering via daemon or direct lookup
		// For now, filter from full history
		let all_sessions = core.history(usize::MAX).await?;
		all_sessions
			.into_iter()
			.filter(|s| sessions.iter().any(|id| s.session_id.contains(id)))
			.collect()
	} else {
		core.history(effective_limit).await?
	};

	if json {
		println!("{}", serde_json::to_string_pretty(&results)?);
		return Ok(());
	}

	if results.is_empty() {
		if !sessions.is_empty() {
			println!("No sessions found matching: {}", sessions.join(", "));
		} else {
			println!("No conversation history found");
		}
		return Ok(());
	}

	if sessions.is_empty() {
		println!("Recent conversations:");
	} else {
		println!("Matching sessions:");
	}
	println!();

	for session in &results {
		print_session(session)?;
	}

	Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper Functions - Data Fetching
// ═══════════════════════════════════════════════════════════════════════════════

/// Get all Claude windows with fingerprints extracted from scrollback
/// This is used by cmd_list when --details is requested
async fn get_windows_with_fingerprints(core: &BabelCore) -> Result<Vec<ClaudeWindow>> {
	// Get windows via core (handles daemon/direct fallback)
	let mut windows = core.windows().await?;

	// Extract fingerprints for windows that don't have them
	for win in &mut windows {
		if win.fingerprint.is_none() {
			if let Ok(scrollback) = get_scrollback(win.id()) {
				let fp = extract_from_scrollback(&scrollback);
				win.fingerprint = Some(fp);
			}
		}
	}
	Ok(windows)
}

/// Show available Claude windows for user selection
async fn show_available_windows(core: &BabelCore) -> Result<()> {
	let windows = core.windows().await?;

	if windows.is_empty() {
		println!("No Claude windows found");
		return Ok(());
	}

	println!("Available Claude windows:");
	for w in &windows {
		let title = w.title.strip_prefix("✳ ").unwrap_or(&w.title);
		let title: String = title.chars().take(30).collect();
		println!("  {:>5}  {:30}  {}", w.id(), title, w.cwd.display());
	}
	Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Display Helpers - Formatting
// ═══════════════════════════════════════════════════════════════════════════════

/// Print a single window in compact format
pub fn print_window(win: &ClaudeWindow) -> Result<()> {
	let conn = init_db()?;

	// Get overlay metadata if we have a session ID
	let meta = win
		.session_id
		.as_ref()
		.and_then(|id| get_metadata(&conn, id).ok().flatten());

	// Indicators
	let focus_indicator = if win.is_focused { "▸" } else { " " };
	let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);
	let custom_icon = meta.as_ref().and_then(|m| m.icon.as_ref());

	// Activity state indicator - shows what Claude is doing right now
	// ⚡ Thinking (yellow), ⚙ ToolUse (cyan), ◆ AwaitingInput (green), ○ Idle (dim), ● Unknown (blue)
	let (state_icon, state_style) = match win.activity_state {
		Some(ActivityState::Thinking) => ("⚡", Style::new().yellow()),
		Some(ActivityState::ToolUse) => ("⚙", Style::new().cyan()),
		Some(ActivityState::AwaitingInput) => ("◆", Style::new().green()),
		Some(ActivityState::Idle) => ("○", Style::new().dim()),
		Some(ActivityState::Unknown) | None => ("●", Style::new().blue()),
	};

	// Title - strip ✳ prefix if present, use summary from session if available
	let raw_title = win
		.session_info
		.as_ref()
		.and_then(|s| s.summaries.first())
		.map(|s| s.summary.as_str())
		.unwrap_or(&win.title);
	let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

	// Styles
	let dim = Style::new().dim();
	let bold = Style::new().bold();
	let yellow = Style::new().yellow();

	// Build the line
	let id_str = format!("{:>3}", win.id());

	// Unread dot or custom icon
	let marker = if let Some(icon) = custom_icon {
		format!("{} ", icon)
	} else if unread {
		format!("{} ", style("●").yellow())
	} else {
		"  ".to_string()
	};

	// Compact cwd - just the last component or ~ prefix
	let cwd_display = win.cwd
	                     .strip_prefix(dirs::home_dir().unwrap_or_default())
	                     .map(|p| format!("~/{}", p.display()))
	                     .unwrap_or_else(|_| win.cwd.display().to_string());

	// Truncate cwd if too long
	let cwd_short = if cwd_display.len() > 30 {
		format!("…{}", &cwd_display[cwd_display.len() - 28..])
	} else {
		cwd_display
	};

	// Format: " ▸●⚡123 Title                        ~/path"
	// Components: focus | unread/icon | state | id | title | cwd
	print!(" {}{}{}", focus_indicator, marker, state_style.apply_to(state_icon));
	print!("{} ", if win.is_focused { bold.apply_to(&id_str) } else { dim.apply_to(&id_str) });
	print!("{}", if win.is_focused { yellow.apply_to(title) } else { Style::new().apply_to(title) });
	println!("  {}", dim.apply_to(&cwd_short));

	Ok(())
}

/// Print a single window in detailed format with all metadata
pub fn print_window_detailed(win: &ClaudeWindow) -> Result<()> {
	let conn = init_db()?;

	// Get overlay metadata if we have a session ID
	let meta = win
		.session_id
		.as_ref()
		.and_then(|id| get_metadata(&conn, id).ok().flatten());

	// Styles
	let dim = Style::new().dim();
	let bold = Style::new().bold();
	let yellow = Style::new().yellow();
	let cyan = Style::new().cyan();

	// Title - strip ✳ prefix
	let raw_title = win
		.session_info
		.as_ref()
		.and_then(|s| s.summaries.first())
		.map(|s| s.summary.as_str())
		.unwrap_or(&win.title);
	let title = raw_title.strip_prefix("✳ ").unwrap_or(raw_title);

	// Focus/unread indicators
	let focus_marker = if win.is_focused { "▸ " } else { "  " };
	let unread = !meta.as_ref().map(|m| m.is_read).unwrap_or(true);

	// Header line: focus + ID + title
	print!("{}", focus_marker);
	print!("{} ", if win.is_focused { bold.apply_to(format!("[{}]", win.id())) } else { dim.apply_to(format!("[{}]", win.id())) });
	if win.is_focused {
		println!("{}", yellow.apply_to(title));
	} else {
		println!("{}", title);
	}

	// Details
	let indent = "      ";

	// CWD - full path
	let cwd_display = win.cwd
	                     .strip_prefix(dirs::home_dir().unwrap_or_default())
	                     .map(|p| format!("~/{}", p.display()))
	                     .unwrap_or_else(|_| win.cwd.display().to_string());
	println!("{}{} {}", indent, dim.apply_to("cwd"), cwd_display);

	// Session ID with confidence if fingerprint matched
	if let Some(ref session_id) = win.session_id {
		if let Some(confidence) = win.match_confidence {
			println!("{}{} {} ({:?} confidence)",
			         indent,
			         dim.apply_to("session"),
			         dim.apply_to(session_id),
			         confidence
			);
		} else {
			println!("{}{} {}", indent, dim.apply_to("session"), dim.apply_to(session_id));
		}
	}

	// Fingerprint data if available
	if let Some(ref fp) = win.fingerprint {
		println!("{}{}", indent, dim.apply_to("fingerprint:"));

		if let Some(ref first) = fp.first_prompt {
			let truncated = if first.len() > 50 {
				format!("{}...", &first[..50])
			} else {
				first.clone()
			};
			println!("{}  {} \"{}\"", indent, dim.apply_to("first_prompt:"), truncated);
		}

		if !fp.tool_sequence.is_empty() {
			let tools_str = if fp.tool_sequence.len() > 4 {
				format!("[{}, ... {} total]", fp.tool_sequence[..4].join(", "), fp.tool_sequence.len())
			} else {
				format!("[{}]", fp.tool_sequence.join(", "))
			};
			println!("{}  {} {}", indent, dim.apply_to("tools:"), tools_str);
		}

		if !fp.recent_prompts.is_empty() {
			println!("{}  {} {}", indent, dim.apply_to("prompts:"), fp.recent_prompts.len());
		}
	}

	// Session info (when available)
	if let Some(ref info) = win.session_info {
		// Project path
		let project_display = info.project
		                          .strip_prefix(dirs::home_dir().unwrap_or_default())
		                          .map(|p| format!("~/{}", p.display()))
		                          .unwrap_or_else(|_| info.project.display().to_string());
		println!("{}{} {}", indent, dim.apply_to("project"), project_display);

		// Slug (codename)
		if let Some(ref slug) = info.slug {
			println!("{}{} {}", indent, dim.apply_to("slug"), cyan.apply_to(slug));
		}

		// Message count
		if info.message_count > 0 {
			println!("{}{} {}", indent, dim.apply_to("messages"), info.message_count);
		}

		// First prompt (from JSONL)
		if let Some(ref prompt) = info.first_prompt {
			println!("{}{} \"{}\"", indent, dim.apply_to("first"), dim.apply_to(prompt));
		}

		// Last activity timestamp
		if let Some(ref ts) = info.last_timestamp {
			println!("{}{} {}", indent, dim.apply_to("last"), dim.apply_to(ts));
		}
	}

	// Custom icon
	if let Some(ref icon) = meta.as_ref().and_then(|m| m.icon.as_ref()) {
		println!("{}{} {}", indent, dim.apply_to("icon"), icon);
	}

	// Unread status
	if unread {
		println!("{}{}", indent, yellow.apply_to("● unread"));
	}

	println!(); // Blank line between entries
	Ok(())
}

/// Print a session in compact format for history listings
pub fn print_session(session: &SessionInfo) -> Result<()> {
	let dim = Style::new().dim();

	let summary = session
		.summaries
		.first()
		.map(|s| s.summary.as_str())
		.unwrap_or("(no summary)");

	let slug = session
		.slug
		.as_ref()
		.map(|s| format!(" ({})", s))
		.unwrap_or_default();

	// Compact project path
	let project_display = session.project
	                             .strip_prefix(dirs::home_dir().unwrap_or_default())
	                             .map(|p| format!("~/{}", p.display()))
	                             .unwrap_or_else(|_| session.project.display().to_string());

	println!("  {}{}  {}", summary, dim.apply_to(&slug), dim.apply_to(&project_display));

	Ok(())
}
