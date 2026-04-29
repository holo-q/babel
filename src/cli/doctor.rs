//! System health diagnostics for babel ecosystem
//!
//! Checks:
//! - Daemon status (running, responsive)
//! - Kitty patches (screen geometry, etc.)
//! - Watcher integration (babel-watcher.py loaded)
//! - Socket connectivity

use anyhow::Result;
use std::path::Path;

/// ANSI color codes for output
mod colors {
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

fn ok(msg: &str) {
    println!("  {}✓{} {}", colors::GREEN, colors::RESET, msg);
}

fn warn(msg: &str) {
    println!("  {}!{} {}", colors::YELLOW, colors::RESET, msg);
}

fn fail(msg: &str) {
    println!("  {}✗{} {}", colors::RED, colors::RESET, msg);
}

fn info(msg: &str) {
    println!("  {}→{} {}", colors::CYAN, colors::RESET, msg);
}

fn section(name: &str) {
    println!("\n{}{}{}:", colors::BOLD, name, colors::RESET);
}

/// Check if the babel daemon is running and responsive
async fn check_daemon() -> bool {
    use claude_babel::utility::ipc::{send_request, socket_path};
    use claude_babel::utility::ipc::{Request, Response};

    let socket = socket_path();
    if !socket.exists() {
        fail(&format!("Daemon socket not found: {}", socket.display()));
        info("Run: systemctl --user start babel.service");
        return false;
    }

    match send_request(&Request::List).await {
        Ok(Response::Windows { windows }) => {
            ok(&format!("Daemon running ({} agent panes)", windows.len()));
            true
        }
        Ok(_) => {
            warn("Daemon responded but with unexpected data");
            true
        }
        Err(e) => {
            fail(&format!("Daemon not responding: {}", e));
            info("Run: systemctl --user restart babel.service");
            false
        }
    }
}

/// Check kitty version and patches
fn check_kitty_patches() -> bool {
    use std::process::Command;

    // Get kitty version
    let version = Command::new("kitty")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let version_line = version.lines().next().unwrap_or("unknown");
    info(&format!("Kitty version: {}", version_line));

    // Check if screen geometry is available in ls output
    let ls_output = Command::new("kitten")
        .args(["@", "ls"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());

    let has_screen_geometry = ls_output
        .as_ref()
        .map(|s| s.contains("\"screen\":") && !s.contains("\"screen\": null"))
        .unwrap_or(false);

    if has_screen_geometry {
        ok("Screen geometry patch: active");
        true
    } else {
        warn("Screen geometry patch: not active in running kitty");
        info("Pane-level sorting unavailable (falls back to OS window geometry)");
        info("Install: cd ~/Workspace/Forks/kitty-patched && makepkg -si");
        info("Then restart kitty for changes to take effect");
        false
    }
}

/// Check if babel-watcher.py is configured and sending events
fn check_watcher() -> bool {
    // Check if watcher is configured in kitty.conf
    let kitty_conf = dirs::config_dir()
        .map(|p| p.join("kitty/kitty.conf"))
        .filter(|p| p.exists());

    let watcher_configured = kitty_conf
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            s.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.contains("watcher") && l.contains("babel-watcher")
            })
        })
        .unwrap_or(false);

    if !watcher_configured {
        warn("Watcher not configured in kitty.conf");
        info("Add to kitty.conf: watcher ~/.config/kitty/babel-watcher.py");
        return false;
    }

    ok("Watcher configured in kitty.conf");

    // Check if watcher file exists
    let watcher_path = dirs::config_dir().map(|p| p.join("kitty/babel-watcher.py"));

    if let Some(path) = watcher_path {
        if path.exists() {
            ok(&format!("Watcher module exists: {}", path.display()));
        } else {
            fail(&format!("Watcher module missing: {}", path.display()));
            return false;
        }
    }

    // Check if event socket is bound (daemon is listening)
    let event_socket = Path::new("/tmp/babel-kitty-events.sock");
    if event_socket.exists() {
        ok("Event socket bound: /tmp/babel-kitty-events.sock");
        true
    } else {
        warn("Event socket not bound (daemon may need restart)");
        false
    }
}

/// Check kitty socket connectivity
fn check_kitty_sockets() -> bool {
    use claude_babel::kitty::find_all_sockets;

    let sockets = find_all_sockets();
    if sockets.is_empty() {
        fail("No kitty sockets found");
        info("Is kitty running with remote control enabled?");
        info("Check: allow_remote_control true in kitty.conf");
        return false;
    }

    ok(&format!("Found {} kitty socket(s)", sockets.len()));

    for socket in &sockets {
        let short = socket.rsplit('/').next().unwrap_or(socket);
        info(&format!("  {}", short));
    }

    true
}

/// Check harness hook integration
fn check_hooks() -> bool {
    let mut any_installed = false;

    if let Some(path) = dirs::home_dir().map(|p| p.join(".claude/settings.json")) {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let has_stop_hook =
                        content.contains("\"Stop\"") && content.contains("babel/hooks/on-stop");
                    let has_prompt_hook = content.contains("\"UserPromptSubmit\"")
                        && content.contains("babel/hooks/on-prompt");
                    let hook_count = [
                        "on-stop",
                        "on-prompt",
                        "on-tool-pre",
                        "on-tool-post",
                        "on-notification",
                        "on-session-start",
                        "on-subagent-stop",
                        "on-pre-compact",
                    ]
                    .iter()
                    .filter(|h| content.contains(*h))
                    .count();

                    if hook_count == 8 {
                        ok("Claude Code hooks installed (8/8)");
                        any_installed = true;
                    } else if has_stop_hook && has_prompt_hook {
                        ok(&format!(
                            "Claude Code core hooks installed ({}/8)",
                            hook_count
                        ));
                        any_installed = true;
                    } else if hook_count > 0 {
                        warn(&format!(
                            "Claude Code hooks partially installed ({}/8)",
                            hook_count
                        ));
                    } else {
                        warn("Claude Code hooks not installed");
                    }
                }
                Err(e) => fail(&format!("Could not read Claude settings.json: {}", e)),
            }
        } else {
            warn("Claude settings.json not found");
        }
    } else {
        warn("Could not determine Claude settings path");
    }

    if let Some(path) = dirs::home_dir().map(|p| p.join(".codex/config.toml")) {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if content.contains("babel/hooks/codex-notify") {
                        ok("Codex notify installed");
                        any_installed = true;
                    } else if content.contains("notify") {
                        warn("Codex notify configured, but not via babel");
                    } else {
                        warn("Codex notify not installed");
                    }
                }
                Err(e) => fail(&format!("Could not read Codex config.toml: {}", e)),
            }
        } else {
            warn("Codex config.toml not found");
        }
    } else {
        warn("Could not determine Codex config path");
    }

    if !any_installed {
        info("Run: babel hook install");
    }

    any_installed
}

/// Main doctor command
pub async fn cmd_doctor() -> Result<()> {
    println!(
        "{}babel doctor{} - System Health Check",
        colors::BOLD,
        colors::RESET
    );
    println!(
        "{}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{}",
        colors::DIM,
        colors::RESET
    );

    let mut all_ok = true;

    section("Daemon");
    if !check_daemon().await {
        all_ok = false;
    }

    section("Kitty Sockets");
    if !check_kitty_sockets() {
        all_ok = false;
    }

    section("Kitty Patches");
    if !check_kitty_patches() {
        // Not a hard failure - system works without patches
    }

    section("Real-time Events");
    if !check_watcher() {
        // Not a hard failure - falls back to polling
    }

    section("Harness Hooks");
    if !check_hooks() {
        // Not a hard failure - can still work via scrollback analysis
    }

    println!();
    if all_ok {
        println!(
            "{}✓ All critical checks passed{}",
            colors::GREEN,
            colors::RESET
        );
    } else {
        println!(
            "{}! Some issues detected - see above for details{}",
            colors::YELLOW,
            colors::RESET
        );
    }

    Ok(())
}
