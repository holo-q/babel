//! Panel indicator protocol — how babel speaks to panel widgets
//!
//! Typed events for panel indicators (richmon, etc.) to display Claude session state.
//! Each indicator shows dots representing active sessions, colored by activity state.
//!
//! ## Protocol
//!
//! Events are sent as JSON lines over Unix datagram socket.
//! Indicators maintain their own state map, receiving deltas from babel.
//!
//! ## Event Types
//!
//! - `Set`: Add or update a session's indicator (color + workspace)
//! - `Remove`: Session closed, remove its indicator
//! - `Clear`: Reset all indicators (daemon restart, etc.)
//!
//! ## Example Flow
//!
//! ```text
//! babel: {"Set":{"id":"k5","color":"#f0c040","workspace":4}}
//! babel: {"Set":{"id":"k2","color":"#666666","workspace":4}}
//! babel: {"Remove":{"id":"k5"}}
//! ```

use serde::{Deserialize, Serialize};

/// A single indicator update event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    },
    /// Remove an indicator (session closed)
    Remove {
        /// Session identifier to remove
        id: String,
    },
    /// Clear all indicators (full reset)
    Clear,
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

    /// Add a set event
    pub fn set(&mut self, id: impl Into<String>, color: impl Into<String>, workspace: u32) {
        self.events.push(IndicatorEvent::Set {
            id: id.into(),
            color: color.into(),
            workspace,
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
        };
        let json = event.to_json();
        assert!(json.contains("\"type\":\"Set\""));
        assert!(json.contains("\"id\":\"k5\""));
        assert!(json.contains("\"color\":\"#f0c040\""));
    }

    #[test]
    fn test_batch() {
        let mut batch = IndicatorBatch::new();
        batch.set("k5", "#f0c040", 4);
        batch.set("k2", "#666666", 4);
        batch.remove("k3");

        assert_eq!(batch.len(), 3);
        let json = batch.to_json();
        assert!(json.contains("events"));
    }
}
