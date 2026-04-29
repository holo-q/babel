//! Layout Capture and Restoration
//!
//! Handles capturing kitty's split tree structure and rebuilding it after
//! pane reboots. This enables holistic reboot of entire OS windows while
//! preserving the exact split arrangement.
//!
//! ## Split Tree Model
//!
//! Kitty's `layout_state.pairs` is a recursive binary tree:
//! ```json
//! {
//!   "one": 1,                    // left pane ID 1
//!   "two": {
//!     "horizontal": false,       // vertical split
//!     "one": 2,                  // top pane ID 2
//!     "two": 3                   // bottom pane ID 3
//!   }
//! }
//! ```
//!
//! We capture this tree, map pane IDs to session metadata, close everything,
//! spawn fresh sessions, then rebuild the tree with new pane IDs. Kitty's
//! remote-control JSON calls pane leaves "windows"; Babel code treats them
//! as panes unless it is talking about actual OS/window-manager clients.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::kitty;
use crate::wset::PaneGeometry;

// ═══════════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════════

/// A node in the split tree - either a leaf (pane) or a split with children
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SplitNode {
    /// Leaf node - a pane ID.
    Leaf(u64),
    /// Split node - two children with orientation and optional bias
    Split {
        /// true = left/right (vsplit), false = top/bottom (hsplit)
        horizontal: bool,
        /// Split ratio for "one" child (0.0-1.0, where 0.5 = equal split)
        /// When bias is 0.38, "one" gets 38% and "two" gets 62%
        bias: Option<f64>,
        /// First child (left or top)
        one: Box<SplitNode>,
        /// Second child (right or bottom)
        two: Box<SplitNode>,
    },
}

impl SplitNode {
    /// Parse from kitty's pairs JSON
    pub fn from_pairs(value: &Value) -> Option<Self> {
        match value {
            // Leaf node - just a pane ID.
            Value::Number(n) => n.as_u64().map(SplitNode::Leaf),

            // Split node - object with one, two, optional horizontal, and optional bias
            Value::Object(obj) => {
                let one_val = obj.get("one");
                let two_val = obj.get("two");

                match (one_val, two_val) {
                    // Full split with both children
                    (Some(one), Some(two)) => {
                        let one = SplitNode::from_pairs(one)?;
                        let two = SplitNode::from_pairs(two)?;
                        // horizontal defaults to true (vsplit) if not specified
                        let horizontal = obj
                            .get("horizontal")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        // Parse bias - kitty stores as decimal (0.0-1.0), only present if != 0.5
                        let bias = obj.get("bias").and_then(|v| v.as_f64());
                        Some(SplitNode::Split {
                            horizontal,
                            bias,
                            one: Box::new(one),
                            two: Box::new(two),
                        })
                    }
                    // Simple case: { "one": id } for single pane
                    (Some(one), None) => SplitNode::from_pairs(one),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Get all pane IDs in this tree (in-order traversal).
    pub fn pane_ids(&self) -> Vec<u64> {
        match self {
            SplitNode::Leaf(id) => vec![*id],
            SplitNode::Split { one, two, .. } => {
                let mut ids = one.pane_ids();
                ids.extend(two.pane_ids());
                ids
            }
        }
    }

    /// Map pane IDs using a translation table.
    pub fn map_ids(&self, mapping: &HashMap<u64, u64>) -> Self {
        match self {
            SplitNode::Leaf(id) => SplitNode::Leaf(*mapping.get(id).unwrap_or(id)),
            SplitNode::Split {
                horizontal,
                bias,
                one,
                two,
            } => SplitNode::Split {
                horizontal: *horizontal,
                bias: *bias,
                one: Box::new(one.map_ids(mapping)),
                two: Box::new(two.map_ids(mapping)),
            },
        }
    }

    /// Count total panes in tree
    pub fn pane_count(&self) -> usize {
        match self {
            SplitNode::Leaf(_) => 1,
            SplitNode::Split { one, two, .. } => one.pane_count() + two.pane_count(),
        }
    }
}

/// Metadata for a single pane that we need to restore
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnapshot {
    /// Kitty pane ID (will change after reboot).
    #[serde(alias = "window_id")]
    pub pane_id: u64,
    /// agent session ID (persistent, used for resume)
    pub session_id: String,
    /// Working directory
    pub cwd: PathBuf,
    /// Pane title (for display).
    pub title: String,
}

/// Complete snapshot of an OS window's layout
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsWindowSnapshot {
    /// Kitty OS window ID
    pub os_window_id: u64,
    /// X11/Wayland platform window ID
    pub platform_window_id: u64,
    /// Socket this OS window belongs to
    pub socket: String,
    /// The split tree structure
    pub split_tree: SplitNode,
    /// Per-pane metadata indexed by pane_id
    pub panes: HashMap<u64, PaneSnapshot>,
    /// Layout type (usually "splits")
    pub layout: String,
    /// OS window geometry for restoration.
    pub geometry: Option<PaneGeometry>,
    /// Workspace number
    pub workspace: Option<i32>,
}

impl OsWindowSnapshot {
    /// Get all pane IDs in this snapshot.
    pub fn pane_ids(&self) -> Vec<u64> {
        self.split_tree.pane_ids()
    }

    /// Check if this snapshot contains a specific pane ID.
    pub fn contains(&self, pane_id: u64) -> bool {
        self.panes.contains_key(&pane_id)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Layout Capture
// ═══════════════════════════════════════════════════════════════════════════════

/// Capture the full layout state of an OS window containing the given pane
pub async fn capture_os_window_layout(
    socket: &str,
    target_pane_id: u64,
    agent_panes: &[crate::utility::agent_discovery::AgentPane],
) -> Result<Option<OsWindowSnapshot>> {
    // Get raw kitty state
    let raw_json = kitty::list_panes_raw_on_socket(socket).await?;
    let os_windows: Vec<Value> =
        serde_json::from_str(&raw_json).context("Failed to parse kitty ls output")?;

    // Find the OS window containing our target
    for os_window in &os_windows {
        let os_window_id = os_window.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

        let platform_window_id = os_window
            .get("platform_window_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Search through tabs for our target pane. Kitty's JSON calls
        // pane entries "windows"; keep the raw variable explicit.
        let tabs = os_window
            .get("tabs")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        for tab in tabs {
            let windows = tab
                .get("windows")
                .and_then(|v| v.as_array())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            // Check if this tab contains our target
            let contains_target = windows
                .iter()
                .any(|w| w.get("id").and_then(|v| v.as_u64()) == Some(target_pane_id));

            if !contains_target {
                continue;
            }

            // Found the right tab - extract layout
            let layout = tab
                .get("layout")
                .and_then(|v| v.as_str())
                .unwrap_or("splits")
                .to_string();

            let layout_state = tab.get("layout_state");
            let pairs = layout_state.and_then(|ls| ls.get("pairs"));

            let split_tree = pairs.and_then(SplitNode::from_pairs).unwrap_or_else(|| {
                // Fallback: single pane
                SplitNode::Leaf(target_pane_id)
            });

            // Build pane snapshots for all kitty pane entries in this tab.
            let mut panes = HashMap::new();
            for window in windows {
                let pane_id = window.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

                // Find corresponding AgentPane for session_id
                if let Some(agent_pane) = agent_panes.iter().find(|p| p.id() == pane_id) {
                    if let Some(ref session_id) = agent_pane.session_id {
                        panes.insert(
                            pane_id,
                            PaneSnapshot {
                                pane_id,
                                session_id: session_id.clone(),
                                cwd: agent_pane.cwd.clone(),
                                title: agent_pane.title.clone(),
                            },
                        );
                    }
                }
            }

            // Get geometry
            let geometry = kitty::get_window_geometry(platform_window_id).ok();

            // Get workspace
            let workspace = kitty::get_workspace(platform_window_id);

            return Ok(Some(OsWindowSnapshot {
                os_window_id,
                platform_window_id,
                socket: socket.to_string(),
                split_tree,
                panes,
                layout,
                geometry,
                workspace,
            }));
        }
    }

    Ok(None)
}

/// Capture layouts for all OS windows containing the given pane IDs
pub async fn capture_all_layouts(
    target_ids: &[u64],
    agent_panes: &[crate::utility::agent_discovery::AgentPane],
) -> Result<Vec<OsWindowSnapshot>> {
    let mut snapshots = Vec::new();
    let mut captured_os_windows = std::collections::HashSet::new();

    for &pane_id in target_ids {
        // Find the pane to get its socket
        if let Some(pane) = agent_panes.iter().find(|p| p.id() == pane_id) {
            // Skip if we already captured this OS window
            if captured_os_windows.contains(&pane.os_window_id) {
                continue;
            }

            if let Some(snapshot) =
                capture_os_window_layout(&pane.addr.socket, pane_id, agent_panes).await?
            {
                captured_os_windows.insert(snapshot.os_window_id);
                snapshots.push(snapshot);
            }
        }
    }

    Ok(snapshots)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Layout Restoration
// ═══════════════════════════════════════════════════════════════════════════════

/// Result of rebuilding a layout
#[derive(Debug)]
pub struct RebuildResult {
    /// Mapping from old pane IDs to new pane IDs.
    pub id_mapping: HashMap<u64, u64>,
    /// Number of panes successfully restored
    pub restored: usize,
    /// Panes that failed to restore
    pub failed: Vec<String>,
}

/// Rebuild an OS window's layout from a snapshot
///
/// This is the core restoration logic:
/// 1. Close all existing panes in the OS window
/// 2. Spawn the first session to create a new OS window
/// 3. Recursively build the split tree by launching with --location and --next-to
/// 4. Restore geometry and workspace
pub async fn rebuild_os_window_layout(snapshot: &OsWindowSnapshot) -> Result<RebuildResult> {
    use tokio::time::{sleep, Duration};

    let mut id_mapping: HashMap<u64, u64> = HashMap::new();
    let mut failed: Vec<String> = Vec::new();

    let pane_ids = snapshot.pane_ids();
    if pane_ids.is_empty() {
        return Ok(RebuildResult {
            id_mapping,
            restored: 0,
            failed,
        });
    }

    info!(
        os_window_id = snapshot.os_window_id,
        pane_count = pane_ids.len(),
        "Rebuilding OS window layout"
    );

    // Step 1: Close all existing panes
    for &old_id in &pane_ids {
        if let Err(e) = kitty::close_pane_on_socket(&snapshot.socket, old_id).await {
            warn!(pane_id = old_id, error = %e, "Failed to close pane");
        }
    }

    // Wait for panes to close.
    sleep(Duration::from_millis(300)).await;

    // Step 2: Rebuild the split tree
    // We need to traverse the tree and spawn panes in the right order
    let first_id = pane_ids.first().copied().unwrap_or(0);

    // Spawn the first pane (creates new OS window)
    if let Some(pane_info) = snapshot.panes.get(&first_id) {
        match spawn_session_for_rebuild(&pane_info.session_id, &pane_info.cwd).await {
            Ok(new_id) => {
                id_mapping.insert(first_id, new_id);
                debug!(old_id = first_id, new_id, "Spawned first pane");
            }
            Err(e) => {
                failed.push(format!("{}: {}", first_id, e));
            }
        }
    }

    // Wait for first pane to appear.
    sleep(Duration::from_millis(400)).await;

    // Now rebuild the tree structure
    if let Some(&anchor_id) = id_mapping.get(&first_id) {
        rebuild_tree_recursive(
            &snapshot.split_tree,
            &snapshot.socket,
            snapshot,
            anchor_id,
            &mut id_mapping,
            &mut failed,
            true, // is_first marks that we've already spawned the first
        )
        .await?;
    }

    // Step 3: Restore geometry and workspace
    if let Some(&first_new_id) = id_mapping.values().next() {
        sleep(Duration::from_millis(200)).await;

        // Get the new platform window ID
        if let Ok(Some(pane)) = kitty::get_pane(first_new_id).await {
            // Restore workspace
            if let Some(ws) = snapshot.workspace {
                if let Err(e) = kitty::move_window_to_workspace(pane.platform_window_id, ws) {
                    warn!(workspace = ws, error = %e, "Failed to restore workspace");
                }
            }

            // Restore geometry
            if let Some(ref geom) = snapshot.geometry {
                sleep(Duration::from_millis(100)).await;
                if let Err(e) = kitty::set_window_geometry(pane.platform_window_id, geom) {
                    warn!(error = %e, "Failed to restore geometry");
                }
            }
        }
    }

    let restored = id_mapping.len();
    Ok(RebuildResult {
        id_mapping,
        restored,
        failed,
    })
}

/// Recursively rebuild a split tree
#[async_recursion::async_recursion]
async fn rebuild_tree_recursive(
    node: &SplitNode,
    socket: &str,
    snapshot: &OsWindowSnapshot,
    anchor_id: u64,
    id_mapping: &mut HashMap<u64, u64>,
    failed: &mut Vec<String>,
    skip_first: bool,
) -> Result<()> {
    use tokio::time::{sleep, Duration};

    match node {
        SplitNode::Leaf(old_id) => {
            // Skip if already spawned (the first pane)
            if skip_first && id_mapping.contains_key(old_id) {
                return Ok(());
            }

            // Spawn this pane (no bias for leaf nodes spawned via "after")
            if let Some(pane_info) = snapshot.panes.get(old_id) {
                let location = "after"; // Default, will be overridden by split
                match spawn_session_with_split(
                    socket,
                    &pane_info.session_id,
                    &pane_info.cwd,
                    anchor_id,
                    location,
                    None, // No bias for non-split spawns
                )
                .await
                {
                    Ok(new_id) => {
                        id_mapping.insert(*old_id, new_id);
                        debug!(old_id, new_id, "Spawned pane");
                    }
                    Err(e) => {
                        failed.push(format!("{}: {}", old_id, e));
                    }
                }
            }
        }
        SplitNode::Split {
            horizontal,
            bias,
            one,
            two,
        } => {
            // First, handle the 'one' subtree (left/top)
            rebuild_tree_recursive(
                one, socket, snapshot, anchor_id, id_mapping, failed, skip_first,
            )
            .await?;

            // Get the anchor for 'two' - should be the last window from 'one'
            let one_ids = one.pane_ids();
            let new_anchor = one_ids
                .last()
                .and_then(|id| id_mapping.get(id))
                .copied()
                .unwrap_or(anchor_id);

            // Small delay between splits
            sleep(Duration::from_millis(100)).await;

            // Handle the 'two' subtree (right/bottom), creating the split
            let location = if *horizontal { "vsplit" } else { "hsplit" };

            // Calculate bias for "two" child:
            // - Stored bias is the fraction for "one" (e.g., 0.38 means one=38%, two=62%)
            // - When spawning "two", we want it to take its fraction (1.0 - bias)
            let two_bias = bias.map(|b| 1.0 - b);

            // Spawn the first pane of 'two' with the split and bias
            let two_ids = two.pane_ids();
            if let Some(&first_two_id) = two_ids.first() {
                if let Some(pane_info) = snapshot.panes.get(&first_two_id) {
                    match spawn_session_with_split(
                        socket,
                        &pane_info.session_id,
                        &pane_info.cwd,
                        new_anchor,
                        location,
                        two_bias,
                    )
                    .await
                    {
                        Ok(new_id) => {
                            id_mapping.insert(first_two_id, new_id);
                            debug!(
                                old_id = first_two_id,
                                new_id,
                                location,
                                ?two_bias,
                                "Spawned split pane"
                            );

                            // Recursively handle rest of 'two' if it's a split
                            if let SplitNode::Split { .. } = two.as_ref() {
                                rebuild_tree_recursive(
                                    two, socket, snapshot, new_id, id_mapping, failed, true,
                                )
                                .await?;
                            }
                        }
                        Err(e) => {
                            failed.push(format!("{}: {}", first_two_id, e));
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Spawn an agent session for layout rebuild (first pane, creates new OS window)
async fn spawn_session_for_rebuild(session_id: &str, cwd: &std::path::Path) -> Result<u64> {
    crate::utility::agent_discovery::spawn_agent_session(session_id, cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Failed to detect spawned window"))
}

/// Spawn an agent session with a specific split location relative to an anchor
///
/// The `bias` parameter (0.0-1.0) controls what fraction of space the NEW pane takes.
/// For example, if the original split was 40/60 and we're spawning the "two" child,
/// bias should be 0.6 (60%) so the new pane takes 60% of the available space.
async fn spawn_session_with_split(
    socket: &str,
    session_id: &str,
    cwd: &std::path::Path,
    anchor_id: u64,
    location: &str,    // "vsplit", "hsplit", or "after"
    bias: Option<f64>, // Fraction for new pane (0.0-1.0)
) -> Result<u64> {
    use std::process::Command;
    use tokio::time::{sleep, Duration};

    // Build args - kitty --bias takes a percentage (0-100) for splits layout
    let mut args = vec![
        "@".to_string(),
        "--to".to_string(),
        socket.to_string(),
        "launch".to_string(),
        "--type=window".to_string(),
        "--cwd".to_string(),
        cwd.to_string_lossy().to_string(),
        format!("--location={}", location),
        format!("--next-to=id:{}", anchor_id),
    ];

    // Add bias if specified (convert decimal to percentage)
    if let Some(b) = bias {
        // Kitty --bias is percentage (0-100), we store as decimal (0.0-1.0)
        let percentage = (b * 100.0).round() as i32;
        args.push(format!("--bias={}", percentage));
        debug!(bias = b, percentage, "Applying split bias");
    }

    args.extend([
        "--env".to_string(),
        "SHELL=/usr/bin/bash".to_string(),
        "--".to_string(),
        "claude".to_string(),
        "-r".to_string(),
        session_id.to_string(),
    ]);

    // Use kitten @ launch with --location, --next-to, and optionally --bias
    let output = Command::new("kitten")
        .args(&args)
        .output()
        .context("Failed to execute kitten @ launch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kitten @ launch failed: {}", stderr.trim());
    }

    // Parse the pane ID from output (kitten @ launch prints the new kitty pane ID).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let new_id: u64 = stdout
        .trim()
        .parse()
        .context("Failed to parse pane ID from kitten @ launch output")?;

    // Small delay for pane to initialize.
    sleep(Duration::from_millis(100)).await;

    Ok(new_id)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_pane() {
        let json: Value = serde_json::json!({"one": 1});
        let tree = SplitNode::from_pairs(&json).unwrap();
        assert_eq!(tree.pane_ids(), vec![1]);
    }

    #[test]
    fn test_parse_simple_split() {
        let json: Value = serde_json::json!({
            "one": 1,
            "two": 2
        });
        // This won't parse as a split (no horizontal field)
        let tree = SplitNode::from_pairs(&json);
        assert!(tree.is_some());
    }

    #[test]
    fn test_parse_full_split() {
        let json: Value = serde_json::json!({
            "one": 1,
            "two": {
                "horizontal": false,
                "one": 2,
                "two": 3
            }
        });
        let tree = SplitNode::from_pairs(&json).unwrap();
        assert_eq!(tree.pane_ids(), vec![1, 2, 3]);
    }

    #[test]
    fn test_map_ids() {
        let tree = SplitNode::Split {
            horizontal: true,
            bias: Some(0.4), // 40/60 split
            one: Box::new(SplitNode::Leaf(1)),
            two: Box::new(SplitNode::Leaf(2)),
        };

        let mut mapping = HashMap::new();
        mapping.insert(1, 100);
        mapping.insert(2, 200);

        let mapped = tree.map_ids(&mapping);
        assert_eq!(mapped.pane_ids(), vec![100, 200]);

        // Verify bias is preserved
        if let SplitNode::Split { bias, .. } = mapped {
            assert_eq!(bias, Some(0.4));
        } else {
            panic!("Expected Split node");
        }
    }

    #[test]
    fn test_parse_split_with_bias() {
        let json: Value = serde_json::json!({
            "horizontal": true,
            "bias": 0.38,
            "one": 5,
            "two": 7
        });
        let tree = SplitNode::from_pairs(&json).unwrap();

        if let SplitNode::Split {
            horizontal,
            bias,
            one,
            two,
        } = tree
        {
            assert!(horizontal);
            assert_eq!(bias, Some(0.38));
            assert_eq!(one.pane_ids(), vec![5]);
            assert_eq!(two.pane_ids(), vec![7]);
        } else {
            panic!("Expected Split node");
        }
    }

    #[test]
    fn test_parse_split_default_bias() {
        // When bias is 0.5 (equal split), kitty omits it from JSON
        let json: Value = serde_json::json!({
            "horizontal": false,
            "one": 1,
            "two": 2
        });
        let tree = SplitNode::from_pairs(&json).unwrap();

        if let SplitNode::Split { bias, .. } = tree {
            assert_eq!(bias, None); // No bias means equal split
        } else {
            panic!("Expected Split node");
        }
    }
}
