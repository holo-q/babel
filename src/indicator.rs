//! Panel indicator protocol — how babel speaks to panel widgets
//!
//! Typed events for panel indicators (richmon, etc.) to display agent session state.
//! Each indicator shows dots representing active sessions, colored by activity state.
//!
//! ## Protocol
//!
//! Events are sent as JSON lines over Unix datagram socket.
//! Indicators maintain their own state map, receiving deltas from babel.
//!
//! ## Event Types
//!
//! - `Set`: Add or update a session's indicator (color, ring, workspace)
//! - `Remove`: Session closed, remove its indicator
//! - `Clear`: Reset all indicators (daemon restart, etc.)
//!
//! ## Visual Properties
//!
//! - `color`: Base dot color (hex string, e.g. "#f0c040")
//! - `ring_intensity`: Animated glow during activity (0.0-1.0)
//! - `has_outline`: Whether to show static outline border
//! - `scale`: Size multiplier (1.0 = default)
//!
//! ## Example Flow
//!
//! ```text
//! babel: {"Set":{"id":"k5","color":"#f0c040","workspace":4,"ring_intensity":0.5}}
//! babel: {"Set":{"id":"k2","color":"#666666","workspace":4}}
//! babel: {"Remove":{"id":"k5"}}
//! ```

use serde::{Deserialize, Serialize};

/// A single indicator update event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum IndicatorEvent {
    /// Set or update an indicator
    Set {
        /// Session identifier (kitty window id prefixed with 'k', e.g. "k5")
        id: String,
        /// Hex color for the dot (e.g. "#f0c040")
        color: String,
        /// Workspace number where the session lives
        workspace: u32,
        /// X position on screen for left-to-right sorting (None = use id order)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x_pos: Option<i32>,

        // ═══ Extended visual properties (for cairo renderers) ═══
        /// Ring glow intensity (0.0-1.0) — animated aura during token output
        #[serde(default, skip_serializing_if = "is_zero")]
        ring_intensity: f64,

        /// Whether to show outline border (e.g., for question state)
        #[serde(default, skip_serializing_if = "is_false")]
        has_outline: bool,

        /// Size multiplier (1.0 = default)
        #[serde(default = "default_scale", skip_serializing_if = "is_default_scale")]
        scale: f64,
    },
    /// Remove an indicator (session closed)
    Remove {
        /// Session identifier to remove
        id: String,
    },
    /// Clear all indicators (full reset)
    Clear,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn default_scale() -> f64 {
    1.0
}

fn is_default_scale(v: &f64) -> bool {
    (*v - 1.0).abs() < 0.001
}

/// Batch of indicator events for atomic updates
///
/// When multiple sessions change in the same operation (e.g. workspace switch),
/// they can be batched for efficiency.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndicatorBatch {
    /// Events in this batch
    pub events: Vec<IndicatorEvent>,
}

impl IndicatorBatch {
    /// Create empty batch
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Add a set event with basic properties
    pub fn set(
        &mut self,
        id: impl Into<String>,
        color: impl Into<String>,
        workspace: u32,
        x_pos: Option<i32>,
    ) {
        self.events.push(IndicatorEvent::Set {
            id: id.into(),
            color: color.into(),
            workspace,
            x_pos,
            ring_intensity: 0.0,
            has_outline: false,
            scale: 1.0,
        });
    }

    /// Add a set event with full visual properties
    pub fn set_full(
        &mut self,
        id: impl Into<String>,
        color: impl Into<String>,
        workspace: u32,
        x_pos: Option<i32>,
        ring_intensity: f64,
        has_outline: bool,
        scale: f64,
    ) {
        self.events.push(IndicatorEvent::Set {
            id: id.into(),
            color: color.into(),
            workspace,
            x_pos,
            ring_intensity,
            has_outline,
            scale,
        });
    }

    /// Add a remove event
    pub fn remove(&mut self, id: impl Into<String>) {
        self.events.push(IndicatorEvent::Remove { id: id.into() });
    }

    /// Add a clear event
    pub fn clear(&mut self) {
        self.events.push(IndicatorEvent::Clear);
    }

    /// Check if batch is empty
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Number of events in batch
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Serialize to JSON line
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl IndicatorEvent {
    /// Serialize single event to JSON
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_serialization() {
        let event = IndicatorEvent::Set {
            id: "k5".to_string(),
            color: "#f0c040".to_string(),
            workspace: 4,
            x_pos: Some(100),
            ring_intensity: 0.5,
            has_outline: true,
            scale: 1.0,
        };
        let json = event.to_json();
        assert!(json.contains("\"type\":\"Set\""));
        assert!(json.contains("\"id\":\"k5\""));
        assert!(json.contains("\"color\":\"#f0c040\""));
        assert!(json.contains("\"ring_intensity\":0.5"));
        assert!(json.contains("\"has_outline\":true"));
    }

    #[test]
    fn test_event_skips_defaults() {
        // Default values should be skipped in serialization
        let event = IndicatorEvent::Set {
            id: "k5".to_string(),
            color: "#f0c040".to_string(),
            workspace: 4,
            x_pos: None,
            ring_intensity: 0.0,
            has_outline: false,
            scale: 1.0,
        };
        let json = event.to_json();
        assert!(!json.contains("x_pos"));
        assert!(!json.contains("ring_intensity"));
        assert!(!json.contains("has_outline"));
        assert!(!json.contains("scale"));
    }

    #[test]
    fn test_batch() {
        let mut batch = IndicatorBatch::new();
        batch.set("k5", "#f0c040", 4, Some(100));
        batch.set("k2", "#666666", 4, Some(200));
        batch.remove("k3");

        assert_eq!(batch.len(), 3);
        let json = batch.to_json();
        assert!(json.contains("events"));
    }

    #[test]
    fn test_batch_full() {
        let mut batch = IndicatorBatch::new();
        batch.set_full("k5", "#f0c040", 4, Some(100), 0.5, true, 1.2);

        let json = batch.to_json();
        assert!(json.contains("ring_intensity"));
        assert!(json.contains("has_outline"));
        assert!(json.contains("scale"));
    }
}
