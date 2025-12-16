//! WSet - Workspace Set persistence for babel
//!
//! A WSet captures the complete state of Claude windows across all workspaces,
//! enabling save/restore of the spaceship's wspace configuration.
//!
//! Terminology:
//! - WSet: A named snapshot of all wspaces + claude windows
//! - WSpace: An individual XFCE workspace containing claude windows
//! - Session: A claude conversation (identified by session_id UUID)
//!
//! Storage: ~/.config/claude-babel/wsets/
//! - _current: Single line with name of active wset
//! - {name}.toml: WSet definitions

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::daemon::BabelState;
use crate::utility::claude_discovery::ClaudeWindow;

// ═══════════════════════════════════════════════════════════════════════════════
// Data Types
// ═══════════════════════════════════════════════════════════════════════════════

/// A workspace set - complete snapshot of claude windows across all wspaces
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

/// Configuration for a single workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WSpaceConfig {
    /// XFCE workspace index (0-based)
    pub index: i32,
    /// Optional title for this workspace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Windows in this workspace
    #[serde(default)]
    pub windows: Vec<WindowConfig>,
}

/// Configuration for a single claude window
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    /// Claude session UUID (from ~/.claude/projects/)
    pub session_id: String,
    /// Working directory for the session
    pub cwd: PathBuf,
    /// Window title (for display, not restoration)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
        .join("claude-babel")
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
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        bail!("Invalid wset name '{}': only alphanumeric, dash, underscore allowed", name);
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

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

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

    /// Build a WSet from current babel state
    ///
    /// Groups windows by workspace and captures their session IDs
    pub fn from_babel_state(name: &str, state: &BabelState) -> Self {
        let now = Utc::now();

        // Group windows by workspace
        let mut wspaces_map: HashMap<i32, Vec<WindowConfig>> = HashMap::new();

        for window in state.windows.values() {
            // Skip windows without session IDs - can't restore them
            let session_id = match &window.session_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let workspace = window.workspace.unwrap_or(0);

            let config = WindowConfig {
                session_id,
                cwd: window.cwd.clone(),
                title: Some(window.title.clone()),
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

    /// Build a WSet from a list of ClaudeWindows (for direct mode without daemon)
    pub fn from_windows(name: &str, windows: &[ClaudeWindow], workspace_titles: &HashMap<i32, String>) -> Self {
        let now = Utc::now();

        let mut wspaces_map: HashMap<i32, Vec<WindowConfig>> = HashMap::new();

        for window in windows {
            let session_id = match &window.session_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let workspace = window.workspace.unwrap_or(0);

            let config = WindowConfig {
                session_id,
                cwd: window.cwd.clone(),
                title: Some(window.title.clone()),
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

    /// Save this WSet to disk
    pub fn save(&mut self) -> Result<PathBuf> {
        let path = wset_path(&self.meta.name)?;

        // Update timestamp
        self.meta.updated = Utc::now();

        let toml_str = toml::to_string_pretty(self)
            .context("Failed to serialize WSet to TOML")?;

        fs::write(&path, toml_str)
            .with_context(|| format!("Failed to write {}", path.display()))?;

        tracing::info!(name = %self.meta.name, path = %path.display(), "Saved WSet");

        Ok(path)
    }

    /// Load a WSet from disk by name
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

        fs::remove_file(&path)
            .with_context(|| format!("Failed to delete {}", path.display()))?;

        // If this was the current wset, clear _current
        if let Ok(Some(current)) = get_current_wset_name() {
            if current == name {
                let current_path = current_file_path()?;
                let _ = fs::remove_file(current_path);
            }
        }

        tracing::info!(name = %name, "Deleted WSet");
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

        let toml_str = toml::to_string_pretty(&wset)
            .context("Failed to serialize WSet")?;

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

        tracing::info!(old = %old_name, new = %new_name, "Renamed WSet");
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
                    tracing::warn!(name = %name, error = %e, "Failed to load WSet for listing");
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
