//! WSet (workspace set) commands
//!
//! Manages saved workspace layouts - capture and restore Claude sessions across
//! multiple kitty windows and XFCE workspaces.

use std::io::Write;
use anyhow::Result;
use claude_babel::core::BabelCore;
use claude_babel::utility::ipc::{send_request, Request, Response};

// Re-export WSetCommands from main for now (will be moved to cli module later)
use super::WSetCommands;

// ═══════════════════════════════════════════════════════════════════════════════
// WSet Commands
// ═══════════════════════════════════════════════════════════════════════════════

/// Save current workspace layout to a WSet file
///
/// Captures all Claude panes across all workspaces with their positions.
pub async fn cmd_wset_save(core: &BabelCore, name: Option<String>, _force: bool, json: bool) -> Result<()> {
    let wset = core.wset_save(name).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "status": "saved",
            "name": wset.meta.name,
            "wspaces": wset.wspaces.len(),
            "windows": wset.wspaces.iter().map(|ws| ws.windows.len()).sum::<usize>(),
        }))?);
    } else {
        let wspaces = wset.wspaces.len();
        let windows = wset.wspaces.iter().map(|ws| ws.windows.len()).sum::<usize>();
        println!("Saved WSet '{}': {} wspaces, {} windows", wset.meta.name, wspaces, windows);
    }
    Ok(())
}

/// Load a workspace layout from a WSet file
///
/// Restores Claude panes to their saved positions. Shows interactive confirmation
/// unless dry_run is enabled. Sessions that no longer exist are skipped.
pub async fn cmd_wset_load(core: &BabelCore, name: Option<String>, dry_run: bool, _anxious: bool, json: bool) -> Result<()> {
    // First, get a dry-run preview
    let preview_request = Request::WSetLoad { name: name.clone(), dry_run: true };

    let (wset_name, wspaces, windows) = match send_request(&preview_request).await {
        Ok(Response::WSetLoaded { name, wspaces, windows, .. }) => (name, wspaces, windows),
        Ok(Response::Error { message }) => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response from daemon: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    };

    // Get current state for comparison
    let current_windows = core.windows().await?.len();
    let current_name = core.wset_current().await?.unwrap_or_else(|| "(none)".to_string());

    if dry_run {
        // Just show preview
        if json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "status": "preview",
                "name": wset_name,
                "wspaces": wspaces,
                "windows": windows,
                "current_name": current_name,
                "current_windows": current_windows,
            }))?);
        } else {
            println!("Would load WSet '{}':", wset_name);
            println!("  {} wspaces, {} windows", wspaces, windows);
            println!("\nCurrent: {} ({} windows)", current_name, current_windows);
            if current_windows > 0 {
                println!("  {} windows would be closed", current_windows);
            }
        }
        return Ok(());
    }

    // Show confirmation box with styled borders
    use console::style;

    println!();
    println!("{}", style("╭─ Load WSet ─────────────────────────────────────────╮").cyan());
    println!("{} {} wspaces {} {} sessions {}",
        style("│").cyan(),
        wspaces,
        style("•").dim(),
        windows,
        style("│").cyan()
    );
    println!("{}{}", style("│").cyan(), style("                                                     │").cyan());
    println!("{} Current: {} ({} windows) {}",
        style("│").cyan(),
        style(&current_name).yellow(),
        current_windows,
        style("│").cyan()
    );
    if current_windows > 0 {
        println!("{} {} {} windows will be closed {}",
            style("│").cyan(),
            style("⚠").yellow(),
            current_windows,
            style("│").cyan()
        );
    }
    println!("{}{}", style("│").cyan(), style("                                                     │").cyan());
    println!("{} {} confirm   {} abort {}",
        style("│").cyan(),
        style("[Enter]").green(),
        style("[Ctrl-C]").red(),
        style("│").cyan()
    );
    println!("{}", style("╰─────────────────────────────────────────────────────╯").cyan());
    println!();

    // Wait for Enter
    use std::io::{self, BufRead};
    print!("  ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;

    // Execute the load
    let request = Request::WSetLoad { name, dry_run: false };

    match send_request(&request).await {
        Ok(Response::WSetLoaded { name, wspaces, windows, skipped, dry_run: _ }) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "status": "loaded",
                    "name": name,
                    "wspaces": wspaces,
                    "windows": windows,
                    "skipped": skipped,
                }))?);
            } else {
                let skipped_msg = if skipped.is_empty() {
                    String::new()
                } else {
                    format!(" ({} skipped)", skipped.len())
                };
                println!("Loaded WSet '{}': {} wspaces, {} windows{}",
                    name, wspaces, windows, skipped_msg);

                if !skipped.is_empty() {
                    println!("\nSkipped sessions (file not found):");
                    for s in &skipped {
                        println!("  - {}", s);
                    }
                }
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("error: unexpected response from daemon: {:?}", other);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: daemon not running or connection failed: {}", e);
            std::process::exit(1);
        }
    }
}

/// Main WSet command dispatcher
///
/// Routes wset subcommands (save, load, list, rename, delete, describe) to their handlers.
pub async fn cmd_wset(core: &BabelCore, command: WSetCommands, json: bool) -> Result<()> {
    match command {
        WSetCommands::Save { name, force } => {
            cmd_wset_save(core, name, force, json).await
        }

        WSetCommands::Load { name, dry_run, anxious } => {
            cmd_wset_load(core, name, dry_run, anxious, json).await
        }

        WSetCommands::List => {
            let wsets = core.wset_list().await?;
            let current = core.wset_current().await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "wsets": wsets,
                    "current": current,
                }))?);
            } else if wsets.is_empty() {
                println!("No saved WSet files found");
                println!("Run 'babel save [name]' to create one");
            } else {
                println!("Saved WSet files:");
                println!();
                for wset in &wsets {
                    let marker = if current.as_ref() == Some(&wset.name) { "* " } else { "  " };
                    let desc = wset.description.as_ref()
                        .map(|d| format!(" - {}", d))
                        .unwrap_or_default();
                    println!("{}{:<16} {} wspaces, {} windows{}",
                        marker, wset.name, wset.wspaces, wset.windows, desc);
                }
                if current.is_some() {
                    println!();
                    println!("  * = current");
                }
            }
            Ok(())
        }

        WSetCommands::Rename { old, new } => {
            match send_request(&Request::WSetRename { old: old.clone(), new: new.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "renamed",
                            "old": old,
                            "new": new,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }

        WSetCommands::Delete { name } => {
            match send_request(&Request::WSetDelete { name: name.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "deleted",
                            "name": name,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }

        WSetCommands::Describe { name, description } => {
            match send_request(&Request::WSetDescribe { name: name.clone(), description: description.clone() }).await {
                Ok(Response::Ok { message }) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                            "status": "described",
                            "name": name,
                            "description": description,
                        }))?);
                    } else {
                        println!("{}", message);
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("error: {}", message);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: daemon not running: {}", e);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("error: unexpected response from daemon: {:?}", other);
                    std::process::exit(1);
                }
            }
        }
    }
}
