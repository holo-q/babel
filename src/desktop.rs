//! Desktop window-manager integration (wmctrl, xdotool, xrandr, xfconf)
//!
//! Platform-level operations for manipulating X11 windows that are NOT kitty-specific.
//! These talk to the WM/compositor layer: workspace assignment, geometry read/write,
//! monitor detection, and XFWM4 focus-stealing prevention bypass.
//!
//! Extracted from kitty.rs to decouple terminal-emulator logic from desktop plumbing.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Command;
use vtr::{boundary, effect, trace_error};

use crate::wset::PaneGeometry;

// ═══════════════════════════════════════════════════════════════════════════════
// XFWM4 Focus Stealing Prevention Bypass
// ═══════════════════════════════════════════════════════════════════════════════
//
// XFWM4 has focus stealing prevention enabled by default. This blocks programmatic
// focus requests from `kitten @ focus-window`. To work around this, we temporarily
// disable prevention, focus the window, wait for the WM to process, then restore.
//
// This matches the behavior of `focus-steal wrap` from Lib/focus-steal but
// implemented in pure Rust for daemon compatibility (no nushell dependency).

const XFWM4_CHANNEL: &str = "xfwm4";
const PREVENT_STEALING_PROP: &str = "/general/prevent_focus_stealing";
const ACTIVATE_ACTION_PROP: &str = "/general/activate_action";
/// Delay after focus to let WM process before restoring prevention
pub(crate) const FOCUS_SETTLE_MS: u64 = 100;

/// Get an xfconf property value
fn xfconf_get(property: &str) -> Option<String> {
    boundary!("xfconf", "get", property = property);

    let output = Command::new("xfconf-query")
        .args(["-c", XFWM4_CHANNEL, "-p", property])
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Set an xfconf property value
fn xfconf_set(property: &str, value: &str) -> Result<()> {
    boundary!("xfconf", "set", property = property, value = value);

    let output = Command::new("xfconf-query")
        .args(["-c", XFWM4_CHANNEL, "-p", property, "-s", value])
        .output()
        .context("Failed to execute xfconf-query")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("xfconf-query set failed: {}", stderr);
    }

    effect!("xfconf", "property changed", property = property);
    Ok(())
}

/// State of XFWM4 focus stealing prevention
pub(crate) struct FocusStealingState {
    prevent: Option<String>,
    action: Option<String>,
}

impl FocusStealingState {
    /// Save current state
    fn save() -> Self {
        Self {
            prevent: xfconf_get(PREVENT_STEALING_PROP),
            action: xfconf_get(ACTIVATE_ACTION_PROP),
        }
    }

    /// Restore saved state
    pub(crate) fn restore(self) {
        if let Some(prevent) = self.prevent {
            let _ = xfconf_set(PREVENT_STEALING_PROP, &prevent);
        }
        if let Some(action) = self.action {
            let _ = xfconf_set(ACTIVATE_ACTION_PROP, &action);
        }
    }
}

/// Temporarily disable focus stealing prevention
///
/// Returns a guard that restores the original state on drop.
pub(crate) fn disable_focus_prevention() -> Option<FocusStealingState> {
    let state = FocusStealingState::save();

    // Only proceed if we successfully saved state (xfconf available)
    if state.prevent.is_none() && state.action.is_none() {
        effect!("xfconf", "unavailable");
        return None;
    }

    // Disable prevention and set action to "bring"
    if let Err(e) = xfconf_set(PREVENT_STEALING_PROP, "false") {
        trace_error!("focus prevention disable failed", error = %e);
        return None;
    }
    if let Err(e) = xfconf_set(ACTIVATE_ACTION_PROP, "bring") {
        trace_error!("activate action set failed", error = %e);
    }

    Some(state)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Workspace Operations (wmctrl)
// ═══════════════════════════════════════════════════════════════════════════════

/// Get workspace number for a platform window ID
pub fn get_workspace(platform_window_id: u64) -> Option<i32> {
    boundary!("wmctrl", "list");

    let output = Command::new("wmctrl").args(["-l"]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hex_id = format!("0x{:08x}", platform_window_id);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].to_lowercase() == hex_id.to_lowercase() {
            return parts[1].parse().ok();
        }
    }

    None
}

/// Get all workspace mappings in one call
pub fn get_all_workspaces() -> HashMap<u64, i32> {
    boundary!("wmctrl", "list-all");

    let mut result = HashMap::new();

    let output = match Command::new("wmctrl").args(["-l"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return result,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(window_id) = u64::from_str_radix(parts[0].trim_start_matches("0x"), 16) {
                if let Ok(workspace) = parts[1].parse::<i32>() {
                    result.insert(window_id, workspace);
                }
            }
        }
    }

    result
}

/// Move a window to a specific workspace
pub fn move_window_to_workspace(platform_window_id: u64, workspace: i32) -> Result<()> {
    boundary!(
        "wmctrl",
        "move-to-workspace",
        window = platform_window_id,
        workspace = workspace
    );

    let hex_id = format!("0x{:08x}", platform_window_id);

    let output = Command::new("wmctrl")
        .args(["-i", "-r", &hex_id, "-t", &workspace.to_string()])
        .output()
        .context("Failed to execute 'wmctrl'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("wmctrl move failed: {}", stderr);
    }

    effect!(
        "wm",
        "window moved to workspace",
        window = platform_window_id,
        workspace = workspace
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Window Geometry (for multi-monitor precise restoration)
// ═══════════════════════════════════════════════════════════════════════════════

/// Get window geometry using xdotool
///
/// Returns (x, y, width, height) for precise multi-monitor restoration.
/// Uses xdotool which handles frame/decoration offsets consistently.
pub fn get_window_geometry(platform_window_id: u64) -> Result<PaneGeometry> {
    boundary!("xdotool", "getwindowgeometry", window = platform_window_id);

    let hex_id = format!("0x{:x}", platform_window_id);

    // Get geometry with xdotool
    let output = Command::new("xdotool")
        .args(["getwindowgeometry", "--shell", &hex_id])
        .output()
        .context("Failed to execute 'xdotool'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("xdotool getwindowgeometry failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse shell format output:
    // WINDOW=123456
    // X=100
    // Y=200
    // WIDTH=800
    // HEIGHT=600
    // SCREEN=0
    let mut x = 0i32;
    let mut y = 0i32;
    let mut width = 0u32;
    let mut height = 0u32;

    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "X" => x = value.parse().unwrap_or(0),
                "Y" => y = value.parse().unwrap_or(0),
                "WIDTH" => width = value.parse().unwrap_or(0),
                "HEIGHT" => height = value.parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    // Get monitor name for this position
    let monitor = get_monitor_at_position(x, y);

    Ok(PaneGeometry {
        x,
        y,
        width,
        height,
        monitor,
    })
}

/// Set window geometry using wmctrl
///
/// Moves and resizes window to exact position. The gravity parameter (first value)
/// is 0 to use default positioning.
pub fn set_window_geometry(platform_window_id: u64, geom: &PaneGeometry) -> Result<()> {
    boundary!("wmctrl", "set-geometry", window = platform_window_id);

    let hex_id = format!("0x{:08x}", platform_window_id);

    // wmctrl -i -r <id> -e <gravity>,<x>,<y>,<width>,<height>
    // gravity 0 = use default, -1 = don't change position, etc.
    let geom_str = format!("0,{},{},{},{}", geom.x, geom.y, geom.width, geom.height);

    let output = Command::new("wmctrl")
        .args(["-i", "-r", &hex_id, "-e", &geom_str])
        .output()
        .context("Failed to execute 'wmctrl'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("wmctrl geometry set failed: {}", stderr);
    }

    effect!(
        "wm",
        "window geometry set",
        window = platform_window_id,
        x = geom.x,
        y = geom.y
    );
    Ok(())
}

/// Get monitor name at a given position using xrandr
///
/// Parses xrandr output to find which monitor contains the given coordinates.
/// Returns None if position is outside all monitors or xrandr fails.
fn get_monitor_at_position(x: i32, y: i32) -> Option<String> {
    boundary!("xrandr", "query");

    let output = Command::new("xrandr").args(["--query"]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse xrandr output for connected monitors with geometry
    // Format: "HDMI-1 connected 1920x1080+0+0 ..."
    for line in stdout.lines() {
        if !line.contains(" connected") {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let monitor_name = parts[0];

        // Find geometry pattern: WxH+X+Y
        for part in &parts[2..] {
            if let Some((dims, pos)) = part.split_once('+') {
                if let Some((w, h)) = dims.split_once('x') {
                    let mon_w: i32 = w.parse().ok()?;
                    let mon_h: i32 = h.parse().ok()?;

                    // Parse +X+Y or +X-Y etc.
                    let coords: Vec<&str> = pos.split(|c| c == '+' || c == '-').collect();
                    if coords.len() >= 2 {
                        let mon_x: i32 = coords[0].parse().ok()?;
                        let mon_y: i32 = coords[1].parse().ok()?;

                        // Check if point is within this monitor
                        if x >= mon_x && x < mon_x + mon_w && y >= mon_y && y < mon_y + mon_h {
                            return Some(monitor_name.to_string());
                        }
                    }
                }
            }
        }
    }

    None
}
