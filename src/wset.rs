//! WSet - Workspace Set persistence for babel
//!
//! A WSet captures the arrangement of the choir—remembering where each voice stood
//! when the tower last rested. This enables save/restore of the spaceship's complete
//! workspace configuration, preserving which workers gathered in which chambers.
//!
//! Terminology:
//! - WSet: A remembered arrangement of all workspaces and their agent panes
//! - WSpace: A chamber in the tower where workers gather (XFCE workspace)
//! - Session: A claude conversation (identified by session_id UUID)
//!
//! Storage: ~/.config/babel/wsets/
//! - _current: Single line with name of active wset
//! - {name}.toml: WSet definitions

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use vtr::{effect, trace_error};

use crate::daemon::BabelState;
use crate::utility::agent_discovery::AgentPane;

// ═══════════════════════════════════════════════════════════════════════════════
// Data Types
// ═══════════════════════════════════════════════════════════════════════════════

/// A remembered arrangement—where each worker stood when the tower last rested.
///
/// Captures the complete state of agent panes across all workspaces, preserving
/// which voices gathered in which chambers and their exact positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WSet {
    pub meta: WSetMeta,
    pub wspaces: Vec<WSpaceConfig>,
}

/// Metadata for a WSet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WSetMeta {
    pub name: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A chamber's contents—which voices gathered there.
///
/// Records the configuration of a single workspace, including which agent panes
/// (workers) occupy this chamber and the chamber's designated title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WSpaceConfig {
    /// XFCE workspace index (0-based)
    pub index: i32,
    /// Optional title for this workspace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Windows in this workspace
    #[serde(default)]
    pub windows: Vec<PaneConfig>,
}

/// A single voice's position in the arrangement.
///
/// Records where a claude worker stood—their session identity, working context,
/// and precise spatial coordinates. When the choir reassembles, each voice returns
/// to their remembered place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneConfig {
    /// agent session UUID (from ~/.claude/projects/)
    pub session_id: String,
    /// Working directory for the session
    pub cwd: PathBuf,
    /// Window title (for display, not restoration)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Window geometry for precise multi-monitor restoration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<PaneGeometry>,
}

/// Window geometry for precise restoration
///
/// Captures position and size so windows can be restored to exact locations,
/// critical for multi-monitor setups where workspace index alone isn't enough.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneGeometry {
    /// X position (absolute, from left edge of combined display)
    pub x: i32,
    /// Y position (absolute, from top edge of combined display)
    pub y: i32,
    /// Window width
    pub width: u32,
    /// Window height
    pub height: u32,
    /// Monitor name (e.g., "HDMI-1", "DP-2") for validation on restore
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

/// Summary info for listing wsets
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WSetSummary {
    pub name: String,
    pub wspaces: usize,
    pub windows: usize,
    pub updated: DateTime<Utc>,
    pub description: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Directory Management
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the wsets directory path, creating if needed
pub fn wsets_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("Failed to get config directory")?
        .join("babel")
        .join("wsets");

    if !dir.exists() {
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create wsets directory: {}", dir.display()))?;
    }

    Ok(dir)
}

/// Path to the _current file that tracks active wset
fn current_file_path() -> Result<PathBuf> {
    Ok(wsets_dir()?.join("_current"))
}

/// Path to a specific wset file
fn wset_path(name: &str) -> Result<PathBuf> {
    // Sanitize name - only allow alphanumeric, dash, underscore
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "Invalid wset name '{}': only alphanumeric, dash, underscore allowed",
            name
        );
    }
    Ok(wsets_dir()?.join(format!("{}.toml", name)))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Current WSet Tracking
// ═══════════════════════════════════════════════════════════════════════════════

/// Get the name of the currently active wset
pub fn get_current_wset_name() -> Result<Option<String>> {
    let path = current_file_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;

    let name = content.trim();
    if name.is_empty() {
        Ok(None)
    } else {
        Ok(Some(name.to_string()))
    }
}

/// Set the currently active wset name
pub fn set_current_wset_name(name: &str) -> Result<()> {
    let path = current_file_path()?;
    fs::write(&path, format!("{}\n", name))
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// WSet Operations
// ═══════════════════════════════════════════════════════════════════════════════

impl WSet {
    /// Create a new empty WSet with the given name
    pub fn new(name: &str) -> Self {
        let now = Utc::now();
        Self {
            meta: WSetMeta {
                name: name.to_string(),
                created: now,
                updated: now,
                description: None,
            },
            wspaces: Vec::new(),
        }
    }

    /// Build a WSet from current babel state—committing the choir to memory.
    ///
    /// Groups windows by workspace and captures their session IDs and geometry,
    /// preserving the arrangement so voices can be called back to their places.
    pub fn from_babel_state(name: &str, state: &BabelState) -> Self {
        use crate::kitty::get_window_geometry;

        let now = Utc::now();

        // Group windows by workspace
        let mut wspaces_map: HashMap<i32, Vec<PaneConfig>> = HashMap::new();

        for window in state.panes.values() {
            // Skip windows without session IDs - can't restore them
            let session_id = match &window.session_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let workspace = window.workspace.unwrap_or(0);

            // Capture window geometry for precise restoration
            let geometry = get_window_geometry(window.platform_window_id)
                .map_err(
                    |e| tracing::debug!(window = window.id(), error = %e, "Failed to get geometry"),
                )
                .ok();

            let config = PaneConfig {
                session_id,
                cwd: window.cwd.clone(),
                title: Some(window.title.clone()),
                geometry,
            };

            wspaces_map.entry(workspace).or_default().push(config);
        }

        // Convert to sorted vec of WSpaceConfig
        let mut wspaces: Vec<WSpaceConfig> = wspaces_map
            .into_iter()
            .map(|(index, windows)| WSpaceConfig {
                index,
                title: state.workspace_titles.get(&index).cloned(),
                windows,
            })
            .collect();

        // Sort by workspace index for consistent ordering
        wspaces.sort_by_key(|ws| ws.index);

        Self {
            meta: WSetMeta {
                name: name.to_string(),
                created: now,
                updated: now,
                description: None,
            },
            wspaces,
        }
    }

    /// Build a WSet from a list of AgentPanes—committing the arrangement to memory.
    ///
    /// Captures geometry for precise multi-monitor restoration (for direct mode without daemon).
    pub fn from_windows(
        name: &str,
        windows: &[AgentPane],
        workspace_titles: &HashMap<i32, String>,
    ) -> Self {
        use crate::kitty::get_window_geometry;

        let now = Utc::now();

        let mut wspaces_map: HashMap<i32, Vec<PaneConfig>> = HashMap::new();

        for window in windows {
            let session_id = match &window.session_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let workspace = window.workspace.unwrap_or(0);

            // Capture window geometry for precise restoration
            let geometry = get_window_geometry(window.platform_window_id)
                .map_err(
                    |e| tracing::debug!(window = window.id(), error = %e, "Failed to get geometry"),
                )
                .ok();

            let config = PaneConfig {
                session_id,
                cwd: window.cwd.clone(),
                title: Some(window.title.clone()),
                geometry,
            };

            wspaces_map.entry(workspace).or_default().push(config);
        }

        let mut wspaces: Vec<WSpaceConfig> = wspaces_map
            .into_iter()
            .map(|(index, windows)| WSpaceConfig {
                index,
                title: workspace_titles.get(&index).cloned(),
                windows,
            })
            .collect();

        wspaces.sort_by_key(|ws| ws.index);

        Self {
            meta: WSetMeta {
                name: name.to_string(),
                created: now,
                updated: now,
                description: None,
            },
            wspaces,
        }
    }

    /// Save this WSet to disk—inscribing the arrangement for later recall.
    pub fn save(&mut self) -> Result<PathBuf> {
        let path = wset_path(&self.meta.name)?;

        // Update timestamp
        self.meta.updated = Utc::now();

        let toml_str = toml::to_string_pretty(self).context("Failed to serialize WSet to TOML")?;

        fs::write(&path, toml_str)
            .with_context(|| format!("Failed to write {}", path.display()))?;

        effect!(
            "wset",
            "save",
            name = self.meta.name.as_str(),
            path = path.to_string_lossy().as_ref()
        );

        Ok(path)
    }

    /// Load a WSet from disk—calling the choir back to their remembered places.
    pub fn load(name: &str) -> Result<Self> {
        let path = wset_path(name)?;

        if !path.exists() {
            bail!("WSet '{}' not found at {}", name, path.display());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        let wset: WSet = toml::from_str(&content)
            .with_context(|| format!("Failed to parse WSet from {}", path.display()))?;

        Ok(wset)
    }

    /// Check if a WSet exists by name
    pub fn exists(name: &str) -> Result<bool> {
        Ok(wset_path(name)?.exists())
    }

    /// Delete a WSet by name
    pub fn delete(name: &str) -> Result<()> {
        let path = wset_path(name)?;

        if !path.exists() {
            bail!("WSet '{}' not found", name);
        }

        fs::remove_file(&path).with_context(|| format!("Failed to delete {}", path.display()))?;

        // If this was the current wset, clear _current
        if let Ok(Some(current)) = get_current_wset_name() {
            if current == name {
                let current_path = current_file_path()?;
                let _ = fs::remove_file(current_path);
            }
        }

        effect!("wset", "delete", name = name);
        Ok(())
    }

    /// Rename a WSet
    pub fn rename(old_name: &str, new_name: &str) -> Result<()> {
        let old_path = wset_path(old_name)?;
        let new_path = wset_path(new_name)?;

        if !old_path.exists() {
            bail!("WSet '{}' not found", old_name);
        }

        if new_path.exists() {
            bail!("WSet '{}' already exists", new_name);
        }

        // Load, update name, save to new location
        let mut wset = Self::load(old_name)?;
        wset.meta.name = new_name.to_string();
        wset.meta.updated = Utc::now();

        let toml_str = toml::to_string_pretty(&wset).context("Failed to serialize WSet")?;

        fs::write(&new_path, toml_str)
            .with_context(|| format!("Failed to write {}", new_path.display()))?;

        fs::remove_file(&old_path)
            .with_context(|| format!("Failed to delete old file {}", old_path.display()))?;

        // Update _current if this was the active wset
        if let Ok(Some(current)) = get_current_wset_name() {
            if current == old_name {
                set_current_wset_name(new_name)?;
            }
        }

        effect!("wset", "rename", old = old_name, new = new_name);
        Ok(())
    }

    /// Get total window count across all wspaces
    pub fn window_count(&self) -> usize {
        self.wspaces.iter().map(|ws| ws.windows.len()).sum()
    }

    /// Get summary for this WSet
    pub fn summary(&self) -> WSetSummary {
        WSetSummary {
            name: self.meta.name.clone(),
            wspaces: self.wspaces.len(),
            windows: self.window_count(),
            updated: self.meta.updated,
            description: self.meta.description.clone(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Listing
// ═══════════════════════════════════════════════════════════════════════════════

/// List all saved wsets with summary info
pub fn list_wsets() -> Result<Vec<WSetSummary>> {
    let dir = wsets_dir()?;
    let mut summaries = Vec::new();

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip non-toml files and _current
        if path.extension().map(|e| e != "toml").unwrap_or(true) {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        if let Some(name) = name {
            match WSet::load(&name) {
                Ok(wset) => summaries.push(wset.summary()),
                Err(e) => {
                    trace_error!("wset load failed", name = name, error = %e);
                }
            }
        }
    }

    // Sort by most recently updated
    summaries.sort_by(|a, b| b.updated.cmp(&a.updated));

    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wset_name_validation() {
        assert!(wset_path("valid-name_123").is_ok());
        assert!(wset_path("invalid/name").is_err());
        assert!(wset_path("invalid name").is_err());
        assert!(wset_path("invalid.name").is_err());
    }
}
