//! Overlay Database - User metadata NOT stored in Claude's files
//!
//! This module manages user-specific metadata for Claude sessions:
//! - Custom icons for visual identification
//! - Read/unread status tracking
//! - Chapter navigation history
//! - Personal notes and annotations
//!
//! All data is stored in SQLite at ~/.local/share/babel/overlay.db, separate from
//! Claude's conversation files to avoid pollution and enable independent backups.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::PathBuf;

/// User metadata for a single Claude session
///
/// This is the "overlay" data - enrichments that live outside Claude's native storage.
/// The session_id links to Claude's conversation IDs for correlation.
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    /// Claude conversation ID (UUID) - primary key linking to Claude's data
    pub session_id: String,

    /// Custom icon/emoji for visual identification in UIs (e.g., "🔥", "📊", "🐛")
    pub icon: Option<String>,

    /// Whether this session has been read/reviewed by the user
    /// (useful for marking "inbox zero" style workflows)
    pub is_read: bool,

    /// Chronological list of chapter titles visited during navigation
    /// Enables "back" functionality and tracks exploration patterns
    pub chapter_history: Vec<String>,

    /// Freeform user notes/annotations about this session
    /// (e.g., "refactoring work", "blocked on upstream bug", etc.)
    pub notes: Option<String>,
}

/// SQLite database for session overlay metadata
///
/// Thread-safe for reads, but writes should be serialized at the application level
/// if concurrent access is needed. For single-user CLI tools this is typically fine.
pub struct BabelStorage {
    conn: Connection,
}

impl BabelStorage {
    /// Open or create the overlay database
    ///
    /// Database location: ~/.local/share/babel/overlay.db
    /// Creates directory and initializes schema if needed.
    ///
    /// # Errors
    /// - If ~/.local/share cannot be determined (rare, but possible in exotic setups)
    /// - If directory creation fails (permissions, disk full, etc.)
    /// - If database connection fails (corrupt db, filesystem issues)
    pub fn open() -> Result<Self> {
        let db_path = Self::db_path()?;

        // Ensure parent directory exists (~/.local/share/babel)
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create babel data directory")?;
        }

        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open overlay database at {}", db_path.display()))?;

        let db = Self { conn };
        db.init_schema()
            .context("Failed to initialize database schema")?;

        Ok(db)
    }

    /// Determine the standard database path
    ///
    /// Uses XDG conventions: ~/.local/share/babel/overlay.db on Linux
    fn db_path() -> Result<PathBuf> {
        let data_dir = dirs::data_dir()
            .context("Could not determine user data directory (~/.local/share)")?;
        Ok(data_dir.join("babel").join("overlay.db"))
    }

    /// Initialize database schema if tables don't exist
    ///
    /// Schema design notes:
    /// - session_id is TEXT (Claude UUIDs) for maximum compatibility
    /// - is_read is INTEGER (SQLite doesn't have native bool)
    /// - chapter_history stored as JSON array for flexible list handling
    /// - All metadata fields are nullable except session_id (primary key)
    fn init_schema(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS session_metadata (
                session_id TEXT PRIMARY KEY,
                icon TEXT,
                is_read INTEGER DEFAULT 0,
                chapter_history TEXT,
                notes TEXT
            )",
            [],
        ).context("Failed to create session_metadata table")?;

        // File touches: tracks which files each session has interacted with
        // Built from scrollback parsing - tool calls like Read, Write, Edit
        // Enables queries like "which sessions touched this file?" or
        // "what files has this session modified?"
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS file_touches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                operation TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                touch_count INTEGER DEFAULT 1,
                UNIQUE(session_id, file_path, operation)
            )",
            [],
        ).context("Failed to create file_touches table")?;

        // Index for fast lookup by file path (most common query)
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_file_touches_path ON file_touches(file_path)",
            [],
        ).context("Failed to create file_touches path index")?;

        // Index for session queries
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_file_touches_session ON file_touches(session_id)",
            [],
        ).context("Failed to create file_touches session index")?;

        // Scrollback cursor: tracks incremental reading position per window
        // Uses kitty's logical offset system (monotonic, never resets)
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS scrollback_cursors (
                pane_addr TEXT PRIMARY KEY,
                cursor INTEGER NOT NULL DEFAULT 0,
                session_id TEXT,
                last_updated INTEGER NOT NULL
            )",
            [],
        ).context("Failed to create scrollback_cursors table")?;

        Ok(())
    }

    /// Get metadata for a specific session
    ///
    /// Returns None if no metadata exists (session not yet enriched by user).
    /// This is distinct from a session with default metadata - absence means
    /// the user hasn't interacted with overlay features yet.
    pub fn get_metadata(&self, session_id: &str) -> Result<Option<SessionMetadata>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, icon, is_read, chapter_history, notes
             FROM session_metadata
             WHERE session_id = ?1"
        )?;

        let mut rows = stmt.query(params![session_id])?;

        if let Some(row) = rows.next()? {
            let chapter_history_json: Option<String> = row.get(3)?;
            let chapter_history = if let Some(json) = chapter_history_json {
                serde_json::from_str(&json).unwrap_or_default()
            } else {
                Vec::new()
            };

            Ok(Some(SessionMetadata {
                session_id: row.get(0)?,
                icon: row.get(1)?,
                is_read: row.get::<_, i32>(2)? != 0,  // SQLite INTEGER to bool
                chapter_history,
                notes: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Set custom icon for a session
    ///
    /// Creates metadata row if it doesn't exist (upsert semantics).
    /// Icon should typically be a single emoji or short string for UI display.
    pub fn set_icon(&self, session_id: &str, icon: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, icon)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET icon = ?2",
            params![session_id, icon],
        ).context("Failed to set icon")?;
        Ok(())
    }

    /// Mark session as read
    ///
    /// Useful for "inbox" style workflows where you track which sessions you've reviewed.
    /// Creates metadata row if needed.
    pub fn mark_read(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, is_read)
             VALUES (?1, 1)
             ON CONFLICT(session_id) DO UPDATE SET is_read = 1",
            params![session_id],
        ).context("Failed to mark session as read")?;
        Ok(())
    }

    /// Mark session as unread
    ///
    /// Reverts read status (e.g., if new activity requires re-review).
    pub fn mark_unread(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, is_read)
             VALUES (?1, 0)
             ON CONFLICT(session_id) DO UPDATE SET is_read = 0",
            params![session_id],
        ).context("Failed to mark session as unread")?;
        Ok(())
    }

    /// Add a chapter to the navigation history
    ///
    /// Appends to the chapter_history array, enabling "back" navigation and
    /// tracking exploration patterns. The history is chronological.
    ///
    /// Implementation: Reads current history, appends, writes back as JSON.
    /// This could be optimized with JSON path expressions in SQLite 3.38+,
    /// but we use the portable approach for compatibility.
    pub fn add_chapter(&self, session_id: &str, chapter: &str) -> Result<()> {
        // Get existing history
        let mut history = if let Some(meta) = self.get_metadata(session_id)? {
            meta.chapter_history
        } else {
            Vec::new()
        };

        // Append new chapter
        history.push(chapter.to_string());

        // Serialize and update
        let history_json = serde_json::to_string(&history)
            .context("Failed to serialize chapter history")?;

        self.conn.execute(
            "INSERT INTO session_metadata (session_id, chapter_history)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET chapter_history = ?2",
            params![session_id, history_json],
        ).context("Failed to add chapter to history")?;

        Ok(())
    }

    /// Set freeform notes for a session
    ///
    /// Overwrites existing notes. Pass empty string to clear.
    /// Notes are intended for short annotations like:
    /// - "Refactoring auth module"
    /// - "Blocked on API bug #1234"
    /// - "Ready for review"
    pub fn set_notes(&self, session_id: &str, notes: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, notes)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET notes = ?2",
            params![session_id, notes],
        ).context("Failed to set notes")?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════════════════
    // File Touches API
    // ═══════════════════════════════════════════════════════════════════════════════

    /// Record a file operation (read, write, edit) for a session
    ///
    /// Uses upsert semantics: increments touch_count if already exists.
    /// Timestamps are Unix epoch seconds.
    pub fn record_file_touch(
        &self,
        session_id: &str,
        file_path: &str,
        operation: &str,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT INTO file_touches (session_id, file_path, operation, first_seen_at, last_seen_at, touch_count)
             VALUES (?1, ?2, ?3, ?4, ?4, 1)
             ON CONFLICT(session_id, file_path, operation)
             DO UPDATE SET last_seen_at = ?4, touch_count = touch_count + 1",
            params![session_id, file_path, operation, now],
        ).context("Failed to record file touch")?;
        Ok(())
    }

    /// Get all sessions that touched a specific file
    ///
    /// Returns (session_id, operation, touch_count, last_seen_at) tuples
    /// ordered by last_seen_at descending (most recent first).
    pub fn get_sessions_for_file(&self, file_path: &str) -> Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, operation, touch_count, last_seen_at
             FROM file_touches
             WHERE file_path = ?1
             ORDER BY last_seen_at DESC"
        )?;

        let rows = stmt.query_map(params![file_path], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to collect file touch rows")
    }

    /// Get all files touched by a specific session
    ///
    /// Returns (file_path, operation, touch_count, last_seen_at) tuples
    /// ordered by last_seen_at descending (most recent first).
    pub fn get_files_for_session(&self, session_id: &str) -> Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, operation, touch_count, last_seen_at
             FROM file_touches
             WHERE session_id = ?1
             ORDER BY last_seen_at DESC"
        )?;

        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to collect file touch rows")
    }

    // ═══════════════════════════════════════════════════════════════════════════════
    // Scrollback Cursor API
    // ═══════════════════════════════════════════════════════════════════════════════

    /// Get the scrollback cursor for a pane (for incremental reading)
    ///
    /// Returns (cursor, session_id) or None if not tracked yet.
    pub fn get_scrollback_cursor(&self, pane_addr: &str) -> Result<Option<(u64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT cursor, session_id FROM scrollback_cursors WHERE pane_addr = ?1"
        )?;

        let mut rows = stmt.query(params![pane_addr])?;

        if let Some(row) = rows.next()? {
            Ok(Some((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, Option<String>>(1)?,
            )))
        } else {
            Ok(None)
        }
    }

    /// Update the scrollback cursor for a pane
    pub fn set_scrollback_cursor(
        &self,
        pane_addr: &str,
        cursor: u64,
        session_id: Option<&str>,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT INTO scrollback_cursors (pane_addr, cursor, session_id, last_updated)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(pane_addr)
             DO UPDATE SET cursor = ?2, session_id = ?3, last_updated = ?4",
            params![pane_addr, cursor as i64, session_id, now],
        ).context("Failed to set scrollback cursor")?;
        Ok(())
    }

    /// Remove scrollback cursor for a closed pane
    pub fn remove_scrollback_cursor(&self, pane_addr: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scrollback_cursors WHERE pane_addr = ?1",
            params![pane_addr],
        ).context("Failed to remove scrollback cursor")?;
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════════════════════════
    // Session Metadata API (continued)
    // ═══════════════════════════════════════════════════════════════════════════════

    /// List all sessions with metadata
    ///
    /// Returns all sessions that have any overlay data (icon, notes, history, etc.).
    /// Sessions with no metadata are not included - this filters to "enriched" sessions.
    ///
    /// Useful for:
    /// - Displaying a dashboard of active/tracked sessions
    /// - Finding sessions with specific icons or notes (filter in application layer)
    /// - Backup/export of user annotations
    pub fn list_all(&self) -> Result<Vec<SessionMetadata>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, icon, is_read, chapter_history, notes
             FROM session_metadata
             ORDER BY session_id"
        )?;

        let rows = stmt.query_map([], |row| {
            let chapter_history_json: Option<String> = row.get(3)?;
            let chapter_history = if let Some(json) = chapter_history_json {
                serde_json::from_str(&json).unwrap_or_default()
            } else {
                Vec::new()
            };

            Ok(SessionMetadata {
                session_id: row.get(0)?,
                icon: row.get(1)?,
                is_read: row.get::<_, i32>(2)? != 0,
                chapter_history,
                notes: row.get(4)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("Failed to collect metadata rows")
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Convenience Free Functions
// ═══════════════════════════════════════════════════════════════════════════════
//
// These provide a simpler API for CLI usage where a single global database is fine.

/// Open the default overlay database
pub fn init_db() -> Result<Connection> {
    let db = BabelStorage::open()?;
    Ok(db.conn)
}

/// Get metadata for a session (standalone function)
pub fn get_metadata(conn: &Connection, session_id: &str) -> Result<Option<SessionMetadata>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, icon, is_read, chapter_history, notes
         FROM session_metadata
         WHERE session_id = ?1"
    )?;

    let mut rows = stmt.query(params![session_id])?;

    if let Some(row) = rows.next()? {
        let chapter_history_json: Option<String> = row.get(3)?;
        let chapter_history = if let Some(json) = chapter_history_json {
            serde_json::from_str(&json).unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Some(SessionMetadata {
            session_id: row.get(0)?,
            icon: row.get(1)?,
            is_read: row.get::<_, i32>(2)? != 0,
            chapter_history,
            notes: row.get(4)?,
        }))
    } else {
        Ok(None)
    }
}

/// Set custom icon for a session
pub fn set_icon(conn: &Connection, session_id: &str, icon: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO session_metadata (session_id, icon)
         VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET icon = ?2",
        params![session_id, icon],
    ).context("Failed to set icon")?;
    Ok(())
}

/// Mark session as read
pub fn mark_read(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO session_metadata (session_id, is_read)
         VALUES (?1, 1)
         ON CONFLICT(session_id) DO UPDATE SET is_read = 1",
        params![session_id],
    ).context("Failed to mark session as read")?;
    Ok(())
}

/// Mark session as unread
pub fn mark_unread(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO session_metadata (session_id, is_read)
         VALUES (?1, 0)
         ON CONFLICT(session_id) DO UPDATE SET is_read = 0",
        params![session_id],
    ).context("Failed to mark session as unread")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: Create an in-memory database for testing
    fn test_db() -> BabelStorage {
        let conn = Connection::open_in_memory().unwrap();
        let db = BabelStorage { conn };
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn test_set_and_get_icon() {
        let db = test_db();
        let session_id = "test-session-1";

        db.set_icon(session_id, "🔥").unwrap();

        let meta = db.get_metadata(session_id).unwrap().unwrap();
        assert_eq!(meta.icon, Some("🔥".to_string()));
    }

    #[test]
    fn test_mark_read_unread() {
        let db = test_db();
        let session_id = "test-session-2";

        // Initially unread (or non-existent)
        db.mark_read(session_id).unwrap();
        assert!(db.get_metadata(session_id).unwrap().unwrap().is_read);

        db.mark_unread(session_id).unwrap();
        assert!(!db.get_metadata(session_id).unwrap().unwrap().is_read);
    }

    #[test]
    fn test_chapter_history() {
        let db = test_db();
        let session_id = "test-session-3";

        db.add_chapter(session_id, "Chapter 1").unwrap();
        db.add_chapter(session_id, "Chapter 2").unwrap();
        db.add_chapter(session_id, "Chapter 3").unwrap();

        let meta = db.get_metadata(session_id).unwrap().unwrap();
        assert_eq!(meta.chapter_history, vec!["Chapter 1", "Chapter 2", "Chapter 3"]);
    }

    #[test]
    fn test_set_notes() {
        let db = test_db();
        let session_id = "test-session-4";

        db.set_notes(session_id, "Important session for auth refactor").unwrap();

        let meta = db.get_metadata(session_id).unwrap().unwrap();
        assert_eq!(meta.notes, Some("Important session for auth refactor".to_string()));
    }

    #[test]
    fn test_list_all() {
        let db = test_db();

        db.set_icon("session-1", "🔥").unwrap();
        db.set_icon("session-2", "📊").unwrap();
        db.set_notes("session-3", "Testing").unwrap();

        let all = db.list_all().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|m| m.session_id == "session-1"));
        assert!(all.iter().any(|m| m.session_id == "session-2"));
        assert!(all.iter().any(|m| m.session_id == "session-3"));
    }

    #[test]
    fn test_nonexistent_session() {
        let db = test_db();
        let meta = db.get_metadata("nonexistent").unwrap();
        assert!(meta.is_none());
    }
}
