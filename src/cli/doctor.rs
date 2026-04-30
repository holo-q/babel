//! System health diagnostics for babel ecosystem
//!
//! Checks:
//! - Daemon status (running, responsive)
//! - Kitty patches (screen geometry, etc.)
//! - Watcher integration (babel-watcher.py loaded)
//! - Socket connectivity

use anyhow::Result;
use claude_babel::core::BabelCore;
use claude_babel::harness_ops::{
    live_panes_from_conflicts, plan_migration, AdapterReadiness, HarnessMigrationReport,
    MigrationDoctorReport, RiskSeverity,
};
use std::path::{Path, PathBuf};

/// ANSI color codes for output
mod colors {
    pub const BLUE: &str = "\x1b[34m";
    pub const GREEN: &str = "\x1b[32m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const RED: &str = "\x1b[31m";
    pub const WHITE: &str = "\x1b[37m";
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

fn styled(style: &str, text: impl std::fmt::Display) -> String {
    format!("{}{}{}", style, text, colors::RESET)
}

fn dim(text: impl std::fmt::Display) -> String {
    styled(colors::DIM, text)
}

fn bold(text: impl std::fmt::Display) -> String {
    styled(colors::BOLD, text)
}

fn cyan(text: impl std::fmt::Display) -> String {
    styled(colors::CYAN, text)
}

fn green(text: impl std::fmt::Display) -> String {
    styled(colors::GREEN, text)
}

fn yellow(text: impl std::fmt::Display) -> String {
    styled(colors::YELLOW, text)
}

fn red(text: impl std::fmt::Display) -> String {
    styled(colors::RED, text)
}

fn magenta(text: impl std::fmt::Display) -> String {
    styled(colors::MAGENTA, text)
}

fn blue(text: impl std::fmt::Display) -> String {
    styled(colors::BLUE, text)
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

/// Command-level doctor for `babel mv --doctor`.
///
/// This is deliberately stronger than a dry-run: it gathers native harness
/// storage evidence, live pane impact, collision risk, and adapter readiness.
/// It does not call the legacy Claude-only mover and never mutates state.
pub async fn cmd_migration_doctor(
    core: &BabelCore,
    source: PathBuf,
    dest: PathBuf,
    json: bool,
) -> Result<()> {
    let source = super::mv::expand_tilde(&source);
    let dest = super::mv::expand_tilde(&dest);

    let conflicts = core.find_panes_in_path(&source).await?;
    let live_panes = live_panes_from_conflicts(&conflicts);
    let report = plan_migration(&source, &dest, live_panes)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_migration_doctor(&report);
    Ok(())
}

fn print_migration_doctor(report: &MigrationDoctorReport) {
    let blocker_count = report
        .risks
        .iter()
        .filter(|risk| matches!(risk.severity, RiskSeverity::Blocker))
        .count();
    let warning_count = report.warning_count();
    let op_count = report.operations().len();

    println!(
        "{}babel mv --doctor{} - Harness Migration Report",
        colors::BOLD,
        colors::RESET
    );
    println!(
        "{}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━{}",
        colors::DIM,
        colors::RESET
    );
    println!(
        "  {} {}",
        dim("summary"),
        format_summary_chips(op_count, blocker_count, warning_count)
    );
    println!("  {} {}", dim("source"), cyan(report.old_path.display()));
    println!("  {}   {}", dim("dest"), cyan(report.new_path.display()));
    println!("  {} {}", dim("policy"), report.indexing_policy);
    println!(
        "  {}   {}",
        dim("mode"),
        yellow("doctor only - no mutation; legacy babel mv is not used")
    );

    section("Live Panes");
    if report.live_panes.is_empty() {
        ok("No live panes under source");
    } else {
        for pane in &report.live_panes {
            let marker = if pane.migratable {
                green("ok")
            } else {
                red("block")
            };
            println!(
                "  {} {:<16} {} {:<5} {:<12} {}",
                marker,
                harness_name(pane.harness),
                dim("pane:"),
                pane.pane_id,
                state_label(&pane.state, pane.migratable),
                cyan(pane.cwd.display())
            );
        }
    }

    section("Harnesses");
    let mut inactive_unsupported = Vec::new();
    for harness in &report.harnesses {
        if is_inactive_unsupported(harness) {
            inactive_unsupported.push(harness.harness.to_string());
            continue;
        }
        print_harness_report(harness);
    }
    if !inactive_unsupported.is_empty() {
        println!(
            "  {} {}",
            dim("unsupported/no-adapter:"),
            dim(inactive_unsupported.join(", "))
        );
    }

    section("Risks");
    if report.risks.is_empty() {
        ok("No risks reported");
    } else {
        for risk in &report.risks {
            let harness = risk
                .harness
                .map(|kind| kind.to_string())
                .unwrap_or_else(|| "global".to_string());
            println!(
                "  {:<16} {:<16} {}",
                severity_label(&risk.severity),
                dim(harness),
                risk.message
            );
        }
    }

    println!();
    println!(
        "{} {}",
        bold("Summary:"),
        format_summary_chips(op_count, blocker_count, warning_count)
    );
}

fn print_harness_report(report: &HarnessMigrationReport) {
    println!(
        "  {:<24} {:<24} {} {:<3} {} {:<3} {} {}",
        harness_name(report.harness),
        readiness_label(&report.readiness),
        dim("sessions:"),
        report.sessions_found,
        dim("refs:"),
        ref_count_label(report.path_references_found),
        dim("ops:"),
        op_count_label(report.operations.len())
    );

    for root in &report.state_roots {
        println!("    {} {}", dim("root"), cyan(root.display()));
    }
    for op in &report.operations {
        let readiness = if op.apply_ready {
            green("apply")
        } else {
            yellow("plan")
        };
        println!(
            "    {} {:<28} {:<7} {} {}",
            magenta("op"),
            bold(&op.action),
            readiness,
            format_operation_target(&op.target),
            dim(format!("({})", op.detail))
        );
    }
    for note in &report.notes {
        println!("    {} {}", blue("note"), dim(note));
    }
}

fn is_inactive_unsupported(report: &HarnessMigrationReport) -> bool {
    report.operations.is_empty()
        && report.path_references_found == 0
        && report.sessions_found == 0
        && report.state_roots.is_empty()
        && matches!(report.readiness, AdapterReadiness::Unsupported)
}

fn format_summary_chips(ops: usize, blockers: usize, warnings: usize) -> String {
    let blockers = if blockers == 0 {
        green(format!("{} blocker(s)", blockers))
    } else {
        red(format!("{} blocker(s)", blockers))
    };
    let warnings = if warnings == 0 {
        green(format!("{} warning(s)", warnings))
    } else {
        yellow(format!("{} warning(s)", warnings))
    };
    format!(
        "{}  {}  {}",
        magenta(format!("{} op(s)", ops)),
        blockers,
        warnings
    )
}

fn readiness_label(readiness: &AdapterReadiness) -> String {
    match readiness {
        AdapterReadiness::ApplyReady => green("apply-ready"),
        AdapterReadiness::DoctorOnly => yellow("doctor-only"),
        AdapterReadiness::ReconOnly => blue("recon-only"),
        AdapterReadiness::Unsupported => dim("unsupported"),
    }
}

fn severity_label(severity: &RiskSeverity) -> String {
    match severity {
        RiskSeverity::Info => blue("info"),
        RiskSeverity::Warning => yellow("warning"),
        RiskSeverity::Blocker => red("blocker"),
    }
}

fn ref_count_label(count: usize) -> String {
    if count == 0 {
        dim(count)
    } else {
        yellow(count)
    }
}

fn op_count_label(count: usize) -> String {
    if count == 0 {
        dim(count)
    } else {
        magenta(count)
    }
}

fn state_label(state: &str, migratable: bool) -> String {
    if !migratable {
        return red(state);
    }
    if state == "Unknown" {
        yellow(state)
    } else {
        green(state)
    }
}

fn harness_name(harness: claude_babel::AgentKind) -> String {
    if let Some((r, g, b)) = parse_hex_color(harness.accent_color()) {
        let brightness = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
        if brightness < 80 {
            return styled(colors::WHITE, harness);
        }
        format!("\x1b[38;2;{};{};{}m{}{}", r, g, b, harness, colors::RESET)
    } else {
        bold(harness)
    }
}

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim();
    let hex = hex.strip_prefix('#')?;
    if hex.len() < 6 {
        return None;
    }
    let hex = &hex[..6];
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn format_operation_target(target: &str) -> String {
    if let Some((from, to)) = target.split_once(" -> ") {
        format!("{} {} {}", cyan(from), dim("->"), cyan(to))
    } else {
        cyan(target)
    }
}
