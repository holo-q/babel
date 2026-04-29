//! LoggingIpcClient - IPC client wrapper that captures all traffic
//!
//! Wraps the standard IPC functions to log all SEND/RECV/EVNT messages
//! for display in the TUI's IPC Log pane. This is the key pattern for
//! debugging IPC and serves as reference for external monitors.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Local};
use tokio::sync::Mutex;

use crate::utility::ipc::{self, Request, Response};

/// Maximum entries in the IPC log ring buffer
const MAX_LOG_ENTRIES: usize = 1000;

/// Direction of IPC traffic
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcDirection {
    /// Request sent to daemon
    Send,
    /// Response received from daemon
    Recv,
    /// Event from daemon subscription (reserved for future event streaming)
    #[allow(dead_code)]
    Event,
}

impl IpcDirection {
    /// Short label for display
    pub fn label(&self) -> &'static str {
        match self {
            IpcDirection::Send => "SEND",
            IpcDirection::Recv => "RECV",
            IpcDirection::Event => "EVNT",
        }
    }
}

/// A single IPC log entry
#[derive(Debug, Clone)]
pub struct IpcLogEntry {
    /// When this message was logged
    pub timestamp: DateTime<Local>,
    /// Direction (SEND/RECV/EVNT)
    pub direction: IpcDirection,
    /// JSON content (may be truncated for display)
    pub content: String,
}

impl IpcLogEntry {
    /// Create a new log entry with current timestamp
    pub fn new(direction: IpcDirection, content: String) -> Self {
        Self {
            timestamp: Local::now(),
            direction,
            content,
        }
    }

    /// Format timestamp as HH:MM:SS.mmm
    pub fn timestamp_str(&self) -> String {
        self.timestamp.format("%H:%M:%S%.3f").to_string()
    }
}

/// IPC client that logs all traffic for debugging
///
/// This wraps the standard IPC functions and captures every message
/// sent and received. The log is a ring buffer that holds up to
/// MAX_LOG_ENTRIES messages.
///
/// Usage pattern for external monitors:
/// ```ignore
/// let client = LoggingIpcClient::new();
/// let response = client.send_request(&Request::List).await?;
/// // The log now contains both the SEND and RECV entries
/// let entries = client.get_log().await;
/// ```
pub struct LoggingIpcClient {
    log: Arc<Mutex<VecDeque<IpcLogEntry>>>,
}

impl LoggingIpcClient {
    /// Create a new logging IPC client
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES))),
        }
    }

    /// Add an entry to the log (ring buffer)
    async fn log_entry(&self, direction: IpcDirection, content: String) {
        let mut log = self.log.lock().await;
        if log.len() >= MAX_LOG_ENTRIES {
            log.pop_front();
        }
        log.push_back(IpcLogEntry::new(direction, content));
    }

    /// Send a request to the daemon and log both SEND and RECV
    pub async fn send_request(&self, request: &Request) -> anyhow::Result<Response> {
        // Log SEND
        let request_json = serde_json::to_string(request)?;
        self.log_entry(IpcDirection::Send, request_json).await;

        // Actual send
        let response = ipc::send_request(request).await?;

        // Log RECV
        let response_json = serde_json::to_string(&response)?;
        self.log_entry(IpcDirection::Recv, response_json).await;

        Ok(response)
    }

    /// Get current log entries (for rendering)
    pub async fn get_log(&self) -> Vec<IpcLogEntry> {
        self.log.lock().await.iter().cloned().collect()
    }

    /// Clear the log
    pub async fn clear_log(&self) {
        self.log.lock().await.clear();
    }
}

impl Default for LoggingIpcClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_log_ring_buffer() {
        let client = LoggingIpcClient::new();

        // Add entries up to limit
        for i in 0..MAX_LOG_ENTRIES + 10 {
            client
                .log_entry(IpcDirection::Send, format!("msg{}", i))
                .await;
        }

        let log = client.get_log().await;
        assert_eq!(log.len(), MAX_LOG_ENTRIES);
        // First entry should be msg10 (first 10 were evicted)
        assert!(log[0].content.contains("msg10"));
    }
}
