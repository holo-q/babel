//! Event notification system for claude-babel daemon
//!
//! Provides pub/sub event broadcasting for GUI frontends (like treasure-panel)
//! to receive push notifications about Claude session state changes.
//!
//! ## Architecture
//!
//! - `EventPublisher`: Owned by daemon, broadcasts events to all subscribers
//! - `BabelEvent`: Enum of all event types (window changes, session matching, etc.)
//! - `EventMessage`: Timestamped, sequenced wrapper around BabelEvent
//! - `EventFilter`: Allows subscribers to filter events by type
//!
//! ## Usage (from daemon)
//!
//! ```rust,ignore
//! let publisher = EventPublisher::new();
//!
//! // Publish events
//! publisher.publish(BabelEvent::WindowAdded {
//!     kitty_id: 42,
//!     title: "claude - workspace".to_string(),
//!     workspace: Some(1),
//! });
//!
//! // Give subscribers to IPC handlers
//! let rx = publisher.subscribe();
//! ```
//!
//! ## Usage (from subscriber - GUI client)
//!
//! ```rust,ignore
//! // Request subscription via IPC
//! let mut rx = get_event_stream()?;
//!
//! while let Ok(msg) = rx.recv().await {
//!     match msg.event {
//!         BabelEvent::WindowAdded { .. } => { /* update UI */ }
//!         BabelEvent::SessionUpdated { .. } => { /* refresh data */ }
//!         _ => {}
//!     }
//! }
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;

// ═══════════════════════════════════════════════════════════════════════════════
// Event Types
// ═══════════════════════════════════════════════════════════════════════════════

/// Events emitted by the babel daemon
///
/// These represent state changes in the Claude session tracking system.
/// All events are broadcast to subscribers via tokio broadcast channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BabelEvent {
    /// New Claude window discovered
    ///
    /// Emitted when kitty window polling detects a new window matching
    /// the Claude Code title pattern (contains "claude" case-insensitive).
    WindowAdded {
        /// Kitty window ID
        kitty_id: u64,
        /// Window title (e.g. "claude - /home/user/project")
        title: String,
        /// XFCE workspace number (1-indexed), None if not on XFCE
        workspace: Option<i32>,
    },

    /// Claude window closed
    ///
    /// Emitted when a previously tracked window no longer appears in kitty ls.
    WindowRemoved {
        /// Kitty window ID that was closed
        kitty_id: u64,
    },

    /// Window gained focus
    ///
    /// Emitted when a Claude window becomes the focused kitty window.
    /// Includes session_id if the window has been matched to a session.
    WindowFocused {
        /// Kitty window ID now focused
        kitty_id: u64,
        /// Session ID if matched, None if still unmatched
        session_id: Option<String>,
    },

    /// Session successfully matched to window via fingerprint
    ///
    /// Emitted when the daemon successfully matches a kitty window to a
    /// Claude session by comparing scrollback content fingerprints.
    SessionMatched {
        /// Kitty window ID that was matched
        kitty_id: u64,
        /// Matched session ID (UUID)
        session_id: String,
        /// Match confidence level: "none", "low", "medium", "high", "exact"
        confidence: String,
    },

    /// Session JSONL file updated (new messages)
    ///
    /// Emitted when filesystem watcher detects changes to a session's
    /// conversation.jsonl file (indicates new AI messages or user input).
    SessionUpdated {
        /// Session ID (UUID) that was updated
        session_id: String,
        /// Project path the session belongs to
        project: String,
    },

    /// Session activity state changed (Idle → Thinking → ToolUse → AwaitingInput)
    ///
    /// Emitted when a Claude window's activity state changes. Enables
    /// external tools (like richspace-babel) to track Claude's activity
    /// and update visual indicators accordingly.
    ///
    /// State detection is based on scrollback pattern analysis and may
    /// have a slight delay (up to 500ms) from actual state changes.
    SessionStateChanged {
        /// Kitty window ID
        kitty_id: u64,
        /// Session ID if matched
        session_id: Option<String>,
        /// XFCE workspace number
        workspace: Option<i32>,
        /// Previous state
        old_state: crate::state::SessionState,
        /// New state
        new_state: crate::state::SessionState,
    },

    /// Workspace ambient title updated via Haiku summarization
    ///
    /// Emitted when a workspace's Claude sessions are summarized into
    /// a human-readable title. Triggered by window add/remove or session match.
    /// Titles are cached (5min TTL) and debounced (10s min between calls).
    WorkspaceTitleUpdated {
        /// XFCE workspace number (1-indexed)
        workspace: i32,
        /// LLM-generated title (2-5 words, e.g. "refactoring auth system")
        title: String,
        /// Number of Claude windows on this workspace
        window_count: usize,
        /// Session ID of most recently active session, if any
        primary_session: Option<String>,
    },

    /// Daemon shutting down
    ///
    /// Final event sent before daemon terminates. Subscribers should
    /// reconnect or exit gracefully.
    DaemonShutdown,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event Message
// ═══════════════════════════════════════════════════════════════════════════════

/// Timestamped event message for subscribers
///
/// Wraps a BabelEvent with metadata for ordering and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    /// UTC timestamp when event occurred
    pub timestamp: DateTime<Utc>,

    /// Monotonically increasing sequence number
    ///
    /// Allows subscribers to detect dropped events (sequence gaps)
    /// if they fall behind the broadcast buffer.
    pub seq: u64,

    /// The event payload
    #[serde(flatten)]
    pub event: BabelEvent,
}

impl EventMessage {
    /// Create a new timestamped event message
    pub fn new(event: BabelEvent, seq: u64) -> Self {
        Self {
            timestamp: Utc::now(),
            seq,
            event,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event Publisher
// ═══════════════════════════════════════════════════════════════════════════════

/// Capacity for broadcast channel (events dropped if subscriber too slow)
///
/// With 100 capacity and events at ~10/sec max, subscribers have ~10s
/// to process before lagging out. Slow subscribers will receive Lagged
/// errors and must resync state.
pub const EVENT_CHANNEL_CAPACITY: usize = 100;

/// Event publisher - owned by daemon, clones Sender to subscribers
///
/// This is the event source. The daemon holds one instance and calls
/// `publish()` when state changes occur. IPC handlers call `subscribe()`
/// to get receivers for clients.
pub struct EventPublisher {
    /// Broadcast sender (cloneable, shared across all subscribers)
    sender: broadcast::Sender<EventMessage>,

    /// Monotonic sequence counter for event ordering
    seq: AtomicU64,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            sender,
            seq: AtomicU64::new(0),
        }
    }

    /// Publish an event to all subscribers
    ///
    /// Returns the number of active subscribers that received the event.
    /// Returns 0 if no subscribers (not an error - events are fire-and-forget).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = publisher.publish(BabelEvent::WindowAdded {
    ///     kitty_id: 42,
    ///     title: "claude".to_string(),
    ///     workspace: None,
    /// });
    /// println!("Event sent to {} subscribers", count);
    /// ```
    pub fn publish(&self, event: BabelEvent) -> usize {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let msg = EventMessage::new(event, seq);

        // send() returns Ok(receiver_count) or Err(_) if no receivers
        // We treat "no receivers" as success (0 count) - not an error
        self.sender.send(msg).unwrap_or(0)
    }

    /// Create a new subscriber receiver
    ///
    /// Returns a broadcast receiver that will receive all future events.
    /// The receiver starts empty - it only sees events published after subscribe().
    ///
    /// # Lagging
    ///
    /// If the subscriber falls behind by more than EVENT_CHANNEL_CAPACITY,
    /// `recv()` will return `Err(RecvError::Lagged(n))` indicating n dropped events.
    /// The client should resync full state when this happens.
    pub fn subscribe(&self) -> broadcast::Receiver<EventMessage> {
        self.sender.subscribe()
    }

    /// Get count of current subscribers
    ///
    /// Useful for daemon health monitoring and debug logs.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventPublisher {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event Filter
// ═══════════════════════════════════════════════════════════════════════════════

/// Filter for subscribing to specific event types
///
/// Allows subscribers to ignore events they don't care about.
/// Empty include list = receive all events (no filtering).
///
/// # Example
///
/// ```rust,ignore
/// // Only receive window-related events
/// let filter = EventFilter::with_events(vec![
///     "window_added".to_string(),
///     "window_removed".to_string(),
///     "window_focused".to_string(),
/// ]);
///
/// while let Ok(msg) = rx.recv().await {
///     if filter.matches(&msg.event) {
///         // Process event
///     }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Event types to include (empty = all events)
    ///
    /// Valid values: "window_added", "window_removed", "window_focused",
    /// "session_matched", "session_updated", "daemon_shutdown"
    pub include: Vec<String>,
}

impl EventFilter {
    /// Create an empty filter (matches all events)
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a filter with specific event types
    pub fn with_events(events: Vec<String>) -> Self {
        Self { include: events }
    }

    /// Check if event passes filter
    ///
    /// Returns true if:
    /// - Filter is empty (include all)
    /// - Event type is in the include list
    pub fn matches(&self, event: &BabelEvent) -> bool {
        if self.include.is_empty() {
            return true;
        }

        let event_name = match event {
            BabelEvent::WindowAdded { .. } => "window_added",
            BabelEvent::WindowRemoved { .. } => "window_removed",
            BabelEvent::WindowFocused { .. } => "window_focused",
            BabelEvent::SessionMatched { .. } => "session_matched",
            BabelEvent::SessionUpdated { .. } => "session_updated",
            BabelEvent::SessionStateChanged { .. } => "session_state_changed",
            BabelEvent::WorkspaceTitleUpdated { .. } => "workspace_title_updated",
            BabelEvent::DaemonShutdown => "daemon_shutdown",
        };

        self.include.iter().any(|e| e == event_name)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Confidence Conversion
// ═══════════════════════════════════════════════════════════════════════════════

// NOTE: Once the fingerprint module exists with MatchConfidence enum,
// uncomment this impl to enable seamless conversion for SessionMatched events:
//
// ```rust,ignore
// use crate::fingerprint::MatchConfidence;
//
// impl From<MatchConfidence> for String {
//     fn from(c: MatchConfidence) -> Self {
//         match c {
//             MatchConfidence::None => "none".to_string(),
//             MatchConfidence::Low => "low".to_string(),
//             MatchConfidence::Medium => "medium".to_string(),
//             MatchConfidence::High => "high".to_string(),
//             MatchConfidence::Exact => "exact".to_string(),
//         }
//     }
// }
// ```
//
// Usage in daemon:
// ```rust,ignore
// publisher.publish(BabelEvent::SessionMatched {
//     kitty_id: 42,
//     session_id: "uuid".to_string(),
//     confidence: match_confidence.into(), // Auto-converts to String
// });
// ```

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_serialization() {
        let event = BabelEvent::WindowAdded {
            kitty_id: 42,
            title: "Test Window".to_string(),
            workspace: Some(1),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("window_added"));
        assert!(json.contains("42"));
        assert!(json.contains("Test Window"));

        // Test round-trip
        let deserialized: BabelEvent = serde_json::from_str(&json).unwrap();
        if let BabelEvent::WindowAdded { kitty_id, .. } = deserialized {
            assert_eq!(kitty_id, 42);
        } else {
            panic!("Deserialization produced wrong variant");
        }
    }

    #[test]
    fn test_event_message_sequencing() {
        let msg1 = EventMessage::new(BabelEvent::DaemonShutdown, 0);
        let msg2 = EventMessage::new(BabelEvent::DaemonShutdown, 1);

        assert_eq!(msg1.seq, 0);
        assert_eq!(msg2.seq, 1);
        assert!(msg1.timestamp <= msg2.timestamp);
    }

    #[test]
    fn test_event_filter_empty() {
        let filter = EventFilter::new();

        let added = BabelEvent::WindowAdded {
            kitty_id: 1,
            title: "".to_string(),
            workspace: None,
        };
        let removed = BabelEvent::WindowRemoved { kitty_id: 1 };
        let shutdown = BabelEvent::DaemonShutdown;

        // Empty filter matches everything
        assert!(filter.matches(&added));
        assert!(filter.matches(&removed));
        assert!(filter.matches(&shutdown));
    }

    #[test]
    fn test_event_filter_selective() {
        let filter = EventFilter::with_events(vec![
            "window_added".to_string(),
            "daemon_shutdown".to_string(),
        ]);

        let added = BabelEvent::WindowAdded {
            kitty_id: 1,
            title: "".to_string(),
            workspace: None,
        };
        let removed = BabelEvent::WindowRemoved { kitty_id: 1 };
        let focused = BabelEvent::WindowFocused {
            kitty_id: 1,
            session_id: None,
        };
        let shutdown = BabelEvent::DaemonShutdown;

        assert!(filter.matches(&added));
        assert!(!filter.matches(&removed));
        assert!(!filter.matches(&focused));
        assert!(filter.matches(&shutdown));
    }

    #[test]
    fn test_publisher_creation() {
        let publisher = EventPublisher::new();
        assert_eq!(publisher.subscriber_count(), 0);
    }

    #[test]
    fn test_publisher_subscribe() {
        let publisher = EventPublisher::new();

        let _rx1 = publisher.subscribe();
        assert_eq!(publisher.subscriber_count(), 1);

        let _rx2 = publisher.subscribe();
        assert_eq!(publisher.subscriber_count(), 2);
    }

    #[test]
    fn test_publisher_no_subscribers() {
        let publisher = EventPublisher::new();

        // Publishing with no subscribers returns 0 (not an error)
        let count = publisher.publish(BabelEvent::DaemonShutdown);
        assert_eq!(count, 0);
    }

    // Async test for actual pub/sub - requires tokio runtime
    #[tokio::test]
    async fn test_publisher_subscribe_receive() {
        let publisher = EventPublisher::new();
        let mut rx = publisher.subscribe();

        // Publish after subscribe
        let count = publisher.publish(BabelEvent::WindowAdded {
            kitty_id: 999,
            title: "Test".to_string(),
            workspace: Some(3),
        });

        assert_eq!(count, 1);

        // Receive the event
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.seq, 0);

        if let BabelEvent::WindowAdded { kitty_id, .. } = msg.event {
            assert_eq!(kitty_id, 999);
        } else {
            panic!("Wrong event type");
        }
    }

    #[tokio::test]
    async fn test_publisher_multiple_subscribers() {
        let publisher = EventPublisher::new();
        let mut rx1 = publisher.subscribe();
        let mut rx2 = publisher.subscribe();

        publisher.publish(BabelEvent::DaemonShutdown);

        // Both subscribers should receive the same event
        let msg1 = rx1.recv().await.unwrap();
        let msg2 = rx2.recv().await.unwrap();

        assert_eq!(msg1.seq, msg2.seq);
        assert_eq!(msg1.seq, 0);
    }

    #[tokio::test]
    async fn test_publisher_sequence_increment() {
        let publisher = EventPublisher::new();
        let mut rx = publisher.subscribe();

        publisher.publish(BabelEvent::DaemonShutdown);
        publisher.publish(BabelEvent::DaemonShutdown);
        publisher.publish(BabelEvent::DaemonShutdown);

        let msg1 = rx.recv().await.unwrap();
        let msg2 = rx.recv().await.unwrap();
        let msg3 = rx.recv().await.unwrap();

        assert_eq!(msg1.seq, 0);
        assert_eq!(msg2.seq, 1);
        assert_eq!(msg3.seq, 2);
    }

    #[test]
    fn test_all_event_variants_serialize() {
        use crate::state::SessionState;

        // Ensure all event types can serialize to JSON
        let events = vec![
            BabelEvent::WindowAdded {
                kitty_id: 1,
                title: "test".to_string(),
                workspace: None,
            },
            BabelEvent::WindowRemoved { kitty_id: 2 },
            BabelEvent::WindowFocused {
                kitty_id: 3,
                session_id: Some("uuid".to_string()),
            },
            BabelEvent::SessionMatched {
                kitty_id: 4,
                session_id: "uuid2".to_string(),
                confidence: "high".to_string(),
            },
            BabelEvent::SessionUpdated {
                session_id: "uuid3".to_string(),
                project: "/home/user/proj".to_string(),
            },
            BabelEvent::SessionStateChanged {
                kitty_id: 5,
                session_id: Some("uuid4".to_string()),
                workspace: Some(1),
                old_state: SessionState::Idle,
                new_state: SessionState::Thinking,
            },
            BabelEvent::DaemonShutdown,
        ];

        for event in events {
            let json = serde_json::to_string(&event);
            assert!(json.is_ok(), "Failed to serialize: {:?}", event);
        }
    }

    #[test]
    fn test_session_state_changed_filter() {
        let filter = EventFilter::with_events(vec![
            "session_state_changed".to_string(),
        ]);

        use crate::state::SessionState;

        let state_changed = BabelEvent::SessionStateChanged {
            kitty_id: 1,
            session_id: None,
            workspace: Some(0),
            old_state: SessionState::Idle,
            new_state: SessionState::AwaitingInput,
        };
        let window_added = BabelEvent::WindowAdded {
            kitty_id: 1,
            title: "".to_string(),
            workspace: None,
        };

        assert!(filter.matches(&state_changed));
        assert!(!filter.matches(&window_added));
    }
}
