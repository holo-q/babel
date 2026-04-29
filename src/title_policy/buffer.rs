//! Title buffer for pending JSONL splices
//!
//! Stores generated titles until pane closes and JSONL can be spliced.

use super::GeneratedTitle;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Buffered title awaiting splice
#[derive(Debug, Clone)]
pub struct BufferedTitle {
    pub title: GeneratedTitle,
    pub buffered_at: Instant,
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// Thread-safe buffer for pending titles
pub struct TitleBuffer {
    titles: RwLock<HashMap<String, BufferedTitle>>,
    max_age: Duration,
}

impl TitleBuffer {
    pub fn new() -> Self {
        Self {
            titles: RwLock::new(HashMap::new()),
            max_age: Duration::from_secs(3600), // 1 hour
        }
    }

    /// Store a new title (replaces existing)
    pub fn store(&self, title: GeneratedTitle) {
        let session_id = title.session_id.clone();
        let mut titles = self.titles.write().unwrap();
        titles.insert(
            session_id,
            BufferedTitle {
                title,
                buffered_at: Instant::now(),
                attempts: 0,
                last_error: None,
            },
        );
    }

    /// Take title for splicing (removes from buffer)
    pub fn take(&self, session_id: &str) -> Option<BufferedTitle> {
        self.titles.write().unwrap().remove(session_id)
    }

    /// Peek without removing
    pub fn peek(&self, session_id: &str) -> Option<BufferedTitle> {
        self.titles.read().unwrap().get(session_id).cloned()
    }

    /// Record a splice attempt
    pub fn record_attempt(&self, session_id: &str, error: Option<String>) {
        if let Some(t) = self.titles.write().unwrap().get_mut(session_id) {
            t.attempts += 1;
            t.last_error = error;
        }
    }

    /// Cleanup expired titles
    pub fn cleanup(&self) {
        let mut titles = self.titles.write().unwrap();
        let now = Instant::now();
        titles.retain(|_, t| now.duration_since(t.buffered_at) < self.max_age);
    }

    /// Get count of buffered titles
    pub fn len(&self) -> usize {
        self.titles.read().unwrap().len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for TitleBuffer {
    fn default() -> Self {
        Self::new()
    }
}
