//! Project Migration Command - Move directories while preserving Claude history
//!
//! The `mv` command is a thin CLI wrapper over BabelCore::migrate_project() that handles:
//! - User-facing output and confirmations (--anxious mode)
//! - Path expansion (~ → home directory)
//! - Result formatting (human-readable or JSON)
//!
//! All actual migration logic lives in BabelCore for state coherence.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use claude_babel::core::{BabelCore, ConflictingWindow, MigrateOptions};
use claude_babel::ActivityState;

// ═══════════════════════════════════════════════════════════════════════════════
// Main Command Handler
// ═══════════════════════════════════════════════════════════════════════════════

/// Move a directory while preserving Claude conversation history
///
/// This is a thin CLI wrapper that:
/// 1. Validates and expands paths
/// 2. Shows conflicts and handles user confirmation (--anxious)
/// 3. Calls BabelCore::migrate_project() for the actual work
/// 4. Formats and displays results
pub async fn cmd_mv(
	core: &mut BabelCore,
	source: PathBuf,
	dest: PathBuf,
	dry_run: bool,
	history_only: bool,
	anxious: bool,
	force: bool,
	json: bool,
) -> Result<()> {
	// Expand ~ in paths
	let source = expand_tilde(&source);
	let dest = expand_tilde(&dest);

	// Validate source exists (unless history-only mode)
	let source_exists = source.exists();
	if !source_exists && !history_only {
		bail!("Source directory does not exist: {}\nIf you already moved the directory, use --history-only", source.display());
	}

	// ─────────────────────────────────────────────────────────────────────────────
	// Step 0: Check for conflicting terminals (preview before migration)
	// ─────────────────────────────────────────────────────────────────────────────
	let conflicts = core.find_windows_in_path(&source).await?;

	if !conflicts.is_empty() {
		display_conflicts(&conflicts, &source);

		// Check for blocking active windows
		let active_count = conflicts
			.iter()
			.filter(|c| matches!(c.state, ActivityState::Thinking | ActivityState::ToolUse))
			.count();

		if active_count > 0 && !force {
			println!("{} active window(s) detected - cannot safely migrate.", active_count);
			println!();
			println!("Options:");
			println!("  • Wait for active sessions to finish and retry");
			println!("  • Use --force to move anyway (active sessions will break)");
			bail!("Active Claude terminals in source path");
		}

		if active_count > 0 && force {
			println!("⚠ Warning: {} active terminal(s) will break after move.", active_count);
			println!("  Proceeding anyway due to --force flag.\n");
		}
	}

	// ─────────────────────────────────────────────────────────────────────────────
	// Anxious mode: step-by-step confirmation
	// ─────────────────────────────────────────────────────────────────────────────
	if anxious {
		// Preview what will happen
		println!("Migration plan:");
		println!();

		if !history_only && source_exists {
			println!("Step 1: Move directory");
			println!("  FROM: {}", source.display());
			println!("  TO:   {}", dest.display());
			println!();
		}

		let migratable = conflicts
			.iter()
			.filter(|c| matches!(c.state, ActivityState::Idle | ActivityState::AwaitingInput))
			.count();
		if migratable > 0 {
			println!("Step 2: Migrate {} idle terminal(s)", migratable);
			for c in &conflicts {
				if matches!(c.state, ActivityState::Idle | ActivityState::AwaitingInput) {
					let new_cwd = dest.join(&c.relative_path);
					println!("  id:{} → cd {}", c.window.id(), new_cwd.display());
				}
			}
			println!();
		}

		println!("Step 3: Update Claude history");
		println!("  Update ~/.claude/projects/ folder name");
		println!("  Update ~/.claude/history.jsonl paths");
		println!();

		if !dry_run && !confirm("Proceed with migration?")? {
			bail!("Aborted by user");
		}
	}

	// ─────────────────────────────────────────────────────────────────────────────
	// Execute migration through BabelCore
	// ─────────────────────────────────────────────────────────────────────────────
	let options = MigrateOptions {
		dry_run,
		move_directory: !history_only && source_exists,
		migrate_terminals: true,
		force,
	};

	let outcome = core.migrate_project(&source, &dest, options).await?;

	// ─────────────────────────────────────────────────────────────────────────────
	// Display results
	// ─────────────────────────────────────────────────────────────────────────────
	if json {
		let result = serde_json::json!({
            "dry_run": outcome.dry_run,
            "directory_moved": outcome.directory_moved,
            "terminals_migrated": outcome.terminals_migrated,
            "active_terminals": outcome.active_terminals,
            "project_folder_renamed": outcome.storage.project_folder_renamed,
            "sessions_preserved": outcome.storage.sessions_preserved,
            "history_entries_updated": outcome.storage.history_entries_updated,
        });
		println!("{}", serde_json::to_string_pretty(&result)?);
		return Ok(());
	}

	let prefix = if dry_run { "[DRY RUN] " } else { "" };

	if outcome.directory_moved {
		println!("{}✓ Directory moved", prefix);
	}

	if !outcome.terminals_migrated.is_empty() {
		println!("{}✓ Migrated {} terminal(s): {:?}", prefix, outcome.terminals_migrated.len(), outcome.terminals_migrated);
	}

	if outcome.storage.project_folder_renamed {
		println!("{}✓ Renamed project folder ({} sessions)", prefix, outcome.storage.sessions_preserved);
	}

	if outcome.storage.history_entries_updated > 0 {
		println!("{}✓ Updated {} history entries", prefix, outcome.storage.history_entries_updated);
	}

	if !outcome.active_terminals.is_empty() {
		println!("⚠ {} active terminal(s) not migrated: {:?}", outcome.active_terminals.len(), outcome.active_terminals);
	}

	println!();
	if dry_run {
		println!("Dry run complete. No changes were made.");
	} else {
		println!("Done! Conversation history maintained.");
	}

	Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Display Helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Display conflicting windows in a user-friendly format
fn display_conflicts(conflicts: &[ConflictingWindow], source: &Path) {
	println!("Found {} Claude window(s) in {}:\n", conflicts.len(), source.display());

	for c in conflicts {
		let state_str = match c.state {
			ActivityState::Idle => "[IDLE]  ",
			ActivityState::AwaitingInput => "[AWAIT] ",
			ActivityState::Thinking => "[ACTIVE: Thinking]",
			ActivityState::ToolUse => "[ACTIVE: Tool Use]",
			ActivityState::Unknown => "[UNKNOWN]",
		};
		let title = c.window.title.strip_prefix("✳ ").unwrap_or(&c.window.title);
		let title_short: String = title.chars().take(40).collect();
		println!("  {} id:{:<4} \"{}\"", state_str, c.window.id(), title_short);
	}
	println!();
}

// ═══════════════════════════════════════════════════════════════════════════════
// User Interaction
// ═══════════════════════════════════════════════════════════════════════════════

/// Ask user for y/n confirmation (for --anxious mode)
pub fn confirm(prompt: &str) -> Result<bool> {
	print!("{} [y/N] ", prompt);
	std::io::stdout().flush()?;

	let mut input = String::new();
	std::io::stdin().read_line(&mut input)?;

	let response = input.trim().to_lowercase();
	Ok(response == "y" || response == "yes")
}

// ═══════════════════════════════════════════════════════════════════════════════
// Path Utilities
// ═══════════════════════════════════════════════════════════════════════════════

/// Expand ~ to home directory
pub fn expand_tilde(path: &Path) -> PathBuf {
	if let Ok(stripped) = path.strip_prefix("~") {
		if let Some(home) = dirs::home_dir() {
			return home.join(stripped);
		}
	}
	path.to_path_buf()
}
