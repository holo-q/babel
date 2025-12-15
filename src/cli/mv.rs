//! Project Migration Command - Move directories while preserving Claude history
//!
//! The `mv` command handles two critical aspects of moving a project directory:
//! 1. Physical directory move (rename or copy+delete for cross-filesystem moves)
//! 2. Claude history migration (project folder + history.jsonl updates)
//!
//! Additionally, it detects and migrates any active Claude terminals whose working
//! directory is inside the source path, ensuring they continue working after the move.
//!
//! # Terminal Migration
//! Idle terminals are automatically migrated by sending:
//!   Ctrl-C → cd <new_path> → claude -r <session_id>
//!
//! Active terminals (thinking/tool_use) block the move unless --force is used.
//!
//! # Modes
//! - Normal: Move directory + update history
//! - --history-only: Only update history (directory already moved)
//! - --dry-run: Preview changes without making them
//! - --anxious: Step-by-step confirmation for each operation
//! - --force: Proceed even if active terminals would break

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use claude_babel::claude_storage::{path_to_encoded, migrate_project};
use claude_babel::core::BabelCore;
use claude_babel::discovery::ClaudeWindow;
use claude_babel::kitty::{get_scrollback, send_text};
use claude_babel::state::{detect_state, SessionState};

// ═══════════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════════

/// A Claude window whose working directory conflicts with the move operation
///
/// These windows must be handled before the move - idle ones can be migrated,
/// active ones block the move (unless --force is used).
pub struct ConflictingWindow {
    pub window: ClaudeWindow,
    pub state: SessionState,
    /// Path relative to source directory (for calculating new cwd)
    pub relative_path: PathBuf,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Main Command Handler
// ═══════════════════════════════════════════════════════════════════════════════

/// Move a directory while preserving Claude conversation history
///
/// This command handles two cases:
/// 1. Full move: Move the directory AND update Claude's history
/// 2. History-only: Just update history (if directory was already moved manually)
///
/// With --anxious, each step requires explicit y/n confirmation before proceeding.
/// With --force, move proceeds even if active Claude terminals would break.
pub async fn cmd_mv(
    core: &BabelCore,
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

    // Validate source exists (unless history-only mode allows missing source)
    let source_exists = source.exists();
    if !source_exists && !history_only {
        bail!("Source directory does not exist: {}\n\
               If you already moved the directory, use --history-only",
            source.display());
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Step 0: Check for conflicting terminals
    // ─────────────────────────────────────────────────────────────────────────────
    let conflicts = find_conflicting_windows(core, &source).await?;

    if !conflicts.is_empty() {
        // Partition by state: idle/awaiting can be migrated, thinking/tool_use are active
        let (migratable, active): (Vec<_>, Vec<_>) = conflicts.iter().partition(|c| {
            matches!(c.state, SessionState::Idle | SessionState::AwaitingInput)
        });

        // Display conflicts
        println!("Found {} Claude window(s) in {}:\n", conflicts.len(), source.display());

        for c in &conflicts {
            let state_str = match c.state {
                SessionState::Idle => "[IDLE]  ",
                SessionState::AwaitingInput => "[AWAIT] ",
                SessionState::Thinking => "[ACTIVE: Thinking]",
                SessionState::ToolUse => "[ACTIVE: Tool Use]",
                SessionState::Unknown => "[UNKNOWN]",
            };
            let title = c.window.title.strip_prefix("✳ ").unwrap_or(&c.window.title);
            let title_short: String = title.chars().take(40).collect();
            println!("  {} id:{:<4} \"{}\"", state_str, c.window.kitty_id, title_short);
        }
        println!();

        // Handle active terminals
        if !active.is_empty() {
            if force {
                println!("⚠ Warning: {} active terminal(s) will break after move.", active.len());
                println!("  Proceeding anyway due to --force flag.\n");
            } else {
                println!("{} active window(s) detected - cannot safely migrate.", active.len());
                println!();
                println!("Options:");
                println!("  • Wait for active sessions to finish and retry");
                println!("  • Use --force to move anyway (active sessions will break)");
                bail!("Active Claude terminals in source path");
            }
        }

        // Migrate idle terminals (before the actual move)
        if !migratable.is_empty() {
            if dry_run {
                println!("[DRY RUN] Would migrate {} idle terminal(s):\n", migratable.len());
                for c in &migratable {
                    let new_cwd = dest.join(&c.relative_path);
                    println!("  id:{} → cd {}", c.window.kitty_id, new_cwd.display());
                    if let Some(ref sid) = c.window.session_id {
                        println!("         → claude -r {}", &sid[..sid.len().min(8)]);
                    }
                }
                println!();
            } else {
                println!("{} idle terminal(s) will be migrated.\n", migratable.len());

                for c in &migratable {
                    let new_cwd = dest.join(&c.relative_path);

                    if anxious {
                        println!("Migrate terminal id:{}?", c.window.kitty_id);
                        println!("  cwd: {} → {}", c.window.cwd.display(), new_cwd.display());
                        if !confirm("Proceed?")? {
                            println!("  ⊘ Skipped\n");
                            continue;
                        }
                    }

                    migrate_terminal(&c.window, &new_cwd, anxious)?;

                    if anxious {
                        println!("  ✓ Terminal migrated\n");
                    } else {
                        println!("  Migrated id:{} → {}", c.window.kitty_id, new_cwd.display());
                    }
                }
                println!();
            }
        }
    }

    // Canonicalize paths for accurate project folder lookup
    // source must exist, dest may not exist yet
    let source_canonical = source.canonicalize()
        .unwrap_or_else(|_| source.clone());
    let dest_canonical = dest.canonicalize().unwrap_or_else(|_| {
        if dest.is_absolute() {
            dest.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&dest))
                .unwrap_or_else(|_| dest.clone())
        }
    });

    tracing::debug!(?source, ?dest, "raw paths from CLI");
    tracing::debug!(?source_canonical, ?dest_canonical, "canonicalized paths");

    // Pre-compute paths for anxious mode preview
    let old_encoded = path_to_encoded(&source_canonical);
    let new_encoded = path_to_encoded(&dest_canonical);
    let claude_base = dirs::home_dir().unwrap().join(".claude");
    let old_project_dir = claude_base.join("projects").join(&old_encoded);

    tracing::debug!(%old_encoded, %new_encoded, "encoded folder names");
    tracing::debug!(?old_project_dir, exists = old_project_dir.exists(), "checking project dir");

    // Count sessions once for reuse
    let session_count = if old_project_dir.exists() {
        fs::read_dir(&old_project_dir)
            .map(|e| e.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .count())
            .unwrap_or(0)
    } else {
        0
    };

    // ─────────────────────────────────────────────────────────────────────────────
    // Step 1: Move the actual directory (unless --history-only or --dry-run)
    // ─────────────────────────────────────────────────────────────────────────────
    if !history_only && source_exists {
        if anxious {
            println!("Step 1: Move directory");
            println!("  FROM: {}", source.display());
            println!("  TO:   {}", dest.display());
        }

        if dry_run {
            println!("[DRY RUN] Would move directory:");
            println!("  {} → {}", source.display(), dest.display());
        } else {
            if anxious && !confirm("Proceed with directory move?")? {
                bail!("Aborted by user");
            }

            // Try rename first (same filesystem)
            if fs::rename(&source, &dest).is_err() {
                tracing::debug!("rename failed, falling back to copy+delete");
                if anxious {
                    println!("  (rename failed, will copy+delete for cross-filesystem move)");
                    if !confirm("Proceed with copy+delete?")? {
                        bail!("Aborted by user");
                    }
                }
                // Fall back to copy + delete for cross-filesystem moves
                copy_dir_recursive(&source, &dest)
                    .with_context(|| format!(
                        "Failed to copy {} → {}",
                        source.display(), dest.display()
                    ))?;
                fs::remove_dir_all(&source)
                    .with_context(|| format!("Failed to remove source: {}", source.display()))?;
            }

            println!("  ✓ Directory moved");
        }
        println!();
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Step 2: Migrate Claude's conversation history (project folder + history.jsonl)
    // These are done atomically by migrate_project(), so one confirmation covers both
    // ─────────────────────────────────────────────────────────────────────────────
    let old_project_path = &source_canonical;
    let new_project_path = &dest_canonical;

    tracing::debug!(?old_project_path, ?new_project_path, "paths for migrate_project");

    if anxious {
        println!("Step 2: Update Claude session data");
        if old_project_dir.exists() {
            println!("  Rename: ~/.claude/projects/{}", old_encoded);
            println!("      →   ~/.claude/projects/{}", new_encoded);
            println!("          ({} session files)", session_count);
        }
        println!("  Update: ~/.claude/history.jsonl");
        println!("          {} → {}", source_canonical.display(), dest_canonical.display());
        // Only confirm in non-dry-run mode
        if !dry_run && !confirm("Proceed?")? {
            bail!("Aborted by user");
        }
    }

    println!("{}Updating Claude history...",
        if dry_run { "[DRY RUN] " } else { "" });

    let result = migrate_project(old_project_path, new_project_path, dry_run)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Display results
    if result.project_folder_renamed {
        println!("  ✓ Renamed project folder ({} sessions)", result.sessions_preserved);
    } else if !dry_run {
        println!("  (no project folder at: {})", result.old_folder);
    }

    if result.history_entries_updated > 0 {
        println!("  ✓ Updated {} history entries", result.history_entries_updated);
    } else if !dry_run {
        println!("  (no history entries matched)");
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
// Terminal Discovery & Migration
// ═══════════════════════════════════════════════════════════════════════════════

/// Find all Claude windows whose cwd is inside the source directory
///
/// These windows will break after the move unless migrated. Returns windows
/// partitioned by state - idle ones can be migrated, active ones block the move.
pub async fn find_conflicting_windows(core: &BabelCore, source: &Path) -> Result<Vec<ConflictingWindow>> {
    // Canonicalize to absolute path - kitty's cwd is always absolute, but user may
    // pass relative path like "." or "foo". Without this, starts_with() always fails.
    let source = source.canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap().join(source));

    let windows = core.windows().await?;
    let mut conflicts = Vec::new();

    for win in windows {
        // Check if window's cwd is inside (or equal to) source path
        if win.cwd.starts_with(&source) {
            // Get window state from scrollback
            let state = match get_scrollback(win.kitty_id) {
                Ok(scrollback) => detect_state(&scrollback),
                Err(_) => SessionState::Unknown,
            };

            // Calculate relative path for new cwd computation
            let relative_path = win.cwd
                .strip_prefix(&source)
                .unwrap_or(Path::new(""))
                .to_path_buf();

            conflicts.push(ConflictingWindow {
                window: win,
                state,
                relative_path,
            });
        }
    }

    Ok(conflicts)
}

/// Migrate an idle terminal to the new directory
///
/// Sends: Ctrl-C (ensure clean prompt) → cd <new_path> → claude -r <session_id>
/// This allows the terminal to continue working seamlessly after the directory move.
pub fn migrate_terminal(
    window: &ClaudeWindow,
    new_cwd: &Path,
    anxious: bool,
) -> Result<()> {
    if anxious {
        println!("  → Sending Ctrl-C...");
    }
    send_text(window.kitty_id, "\x03")?; // Ctrl-C
    sleep(Duration::from_millis(100));

    if anxious {
        println!("  → cd {}", new_cwd.display());
    }
    send_text(window.kitty_id, &format!("cd {}\n", shell_escape(new_cwd)))?;
    sleep(Duration::from_millis(50));

    // Resume session if we know the session ID
    if let Some(ref session_id) = window.session_id {
        if anxious {
            println!("  → claude -r {}...", &session_id[..session_id.len().min(8)]);
        }
        send_text(window.kitty_id, &format!("claude -r {}\n", session_id))?;
    }

    Ok(())
}

/// Escape a path for safe shell usage
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    // If path contains special chars, quote it
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('$') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// User Interaction Helpers
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
// Filesystem Utilities
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

/// Recursively copy a directory (for cross-filesystem moves)
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}
