//! The Tower's Long Memory
//!
//! This module is the institutional memory of Babel—what persists when workers sleep,
//! what the tower remembers about each conversation's soul. While Claude's native storage
//! holds the dialogue itself, this is the overlay: the sigils, the witness marks, the notes
//! scrawled in margins by those who tend the tower.
//!
//! The tower remembers:
//! - Icons assigned to sessions—visual sigils for quick recognition
//! - Which conversations have been witnessed (read status)
//! - The path of exploration through each session (chapter history)
//! - Annotations and notes about each worker's purpose
//! - Which files each soul has touched during its work
//!
//! All memory persists in SQLite at ~/.local/share/babel/overlay.db, separate from
//! Claude's conversation files to keep the memories clean and independently preservable.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::PathBuf;

/// What the Tower Remembers About a Soul
///
/// Each session is a worker's soul—the conversation thread where work unfolds.
/// This metadata is what the tower's librarians keep in their ledgers: annotations
/// that exist outside the conversation itself, enrichments that help navigate and
/// understand the workers' collective labor.
///
/// The session_id is the soul's true name (Claude's UUID), linking institutional memory
/// to the actual dialogue preserved in Claude's native storage.
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    /// The soul's true name (Claude conversation UUID)
    /// Links institutional memory to the actual conversation stored by Claude
    pub session_id: String,

    /// The sigil assigned to this soul for quick recognition
    /// Visual markers like "🔥" (urgent work), "📊" (analysis), "🐛" (debugging)
    pub icon: Option<String>,

    /// Whether this conversation has been witnessed by the Captain
    /// The tower tracks which souls have been reviewed, which still await attention
    pub is_read: bool,

    /// The path of exploration through this soul's work
    /// Chronological record of chapters visited—the trail of navigation through memory
    pub chapter_history: Vec<String>,

    /// Annotations about this soul's purpose and progress
    /// Notes scrawled in margins: "refactoring auth", "blocked on API", "ready for review"
    pub notes: Option<String>,
}

/// The Tower's Memory—What Persists When Workers Sleep
///
/// This is Babel's institutional knowledge, the durable substrate that survives restarts
/// and outlives individual sessions. While workers come and go, Claude processes spawn
/// and terminate, the tower remembers: which souls exist, what they've done, which files
/// they've touched, where exploration has led.
///
/// Thread-safe for concurrent reads. Writes should be serialized at the application level
/// if multiple processes need simultaneous access, though for single-user CLI usage the
/// natural serialization of command invocations handles this implicitly.
pub struct BabelStorage {
    conn: Connection,
}

impl BabelStorage {
    /// Awaken the tower's memory or create it anew if this is the first summoning
    ///
    /// Opens the persistent institutional memory at ~/.local/share/babel/overlay.db.
    /// If the tower has never stored memories before, initializes the ledgers (schema).
    /// If the tower already knows things, reconnects to what it remembers.
    ///
    /// # Errors
    /// - If ~/.local/share cannot be determined (rare, exotic system configurations)
    /// - If directory creation fails (permissions, disk space)
    /// - If database connection fails (corruption, filesystem issues)
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

        // The tower's ledger of file interactions—which souls touched which paths
        // Built from scrollback parsing of tool invocations (Read, Write, Edit, etc.)
        // Enables institutional memory queries: "who worked on this?" and "what did they touch?"
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

        // Generated titles: tracks which sessions have babel-generated haiku titles
        // This is how we distinguish "proper" titles from procedural fallbacks.
        // Claude Code may discard extended fields on reserialization, so we maintain
        // our own cache keyed by session_id.
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS generated_titles (
                session_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                generated_at INTEGER NOT NULL,
                source TEXT NOT NULL DEFAULT 'haiku'
            )",
            [],
        ).context("Failed to create generated_titles table")?;

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

    /// Give a soul its sigil—a visual mark for quick recognition
    ///
    /// Assigns or updates the icon associated with this session. The tower inscribes this
    /// into its ledgers, creating a new entry if this soul is not yet known. Sigils are
    /// typically single emoji ("🔥", "📊", "🐛") but can be short strings for UI rendering.
    ///
    /// This is how the Captain marks important work, urgent threads, or thematic categories.
    pub fn set_icon(&self, session_id: &str, icon: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, icon)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET icon = ?2",
            params![session_id, icon],
        ).context("Failed to set icon")?;
        Ok(())
    }

    /// Note that this conversation has been witnessed
    ///
    /// The tower records that the Captain (or another observer) has reviewed this soul's work.
    /// Useful for tracking which conversations await attention and which have been processed—
    /// an institutional memory of "inbox zero" style workflows.
    ///
    /// Creates a memory record if this soul is not yet known to the tower.
    pub fn mark_read(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_metadata (session_id, is_read)
             VALUES (?1, 1)
             ON CONFLICT(session_id) DO UPDATE SET is_read = 1",
            params![session_id],
        ).context("Failed to mark session as read")?;
        Ok(())
    }

    /// Mark that this conversation requires fresh eyes
    ///
    /// Reverts the witnessed status—the tower notes that new activity has emerged or
    /// that re-review is needed. Perhaps the work has continued, or discoveries warrant
    /// another pass. The institutional memory reflects that this soul's work is unfinished.
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
    // File Memory—Remembering What Each Soul Touched
    // ═══════════════════════════════════════════════════════════════════════════════

    /// Remember that a soul touched this file
    ///
    /// The tower records every interaction: reads, writes, edits. This institutional memory
    /// enables queries like "which workers touched this file?" or "what files has this soul
    /// modified?" Built from scrollback analysis—the tower parses tool invocations and notes
    /// what each conversation has handled.
    ///
    /// Uses upsert semantics: if this soul has touched this file before, increments the count
    /// and updates the timestamp. Memory compounds with repeated interaction.
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

    /// Recall which souls have touched this file
    ///
    /// Query institutional memory for all workers who interacted with this path.
    /// Returns (session_id, operation, touch_count, last_seen_at) ordered by recency—
    /// most recent touches first. The tower's ledger of who has handled what.
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

    /// Recall what files this soul has touched
    ///
    /// Query institutional memory for all paths this worker has interacted with.
    /// Returns (file_path, operation, touch_count, last_seen_at) ordered by recency—
    /// most recent touches first. The tower's record of what this soul has handled.
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
    // Generated Titles—Babel's Own Haiku-Generated Names
    // ═══════════════════════════════════════════════════════════════════════════════

    /// Record that babel generated a haiku title for this session
    ///
    /// This marks a session as having a "proper" title (non-procedural).
    /// Used in `babel ls` to style titles: haiku=normal, procedural=dim+italic.
    pub fn set_generated_title(&self, session_id: &str, title: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.conn.execute(
            "INSERT INTO generated_titles (session_id, title, generated_at, source)
             VALUES (?1, ?2, ?3, 'haiku')
             ON CONFLICT(session_id) DO UPDATE SET title = ?2, generated_at = ?3",
            params![session_id, title, now],
        ).context("Failed to set generated title")?;
        Ok(())
    }

    /// Check if babel has a haiku-generated title for this session
    ///
    /// Returns the title if this session has a babel-generated title,
    /// None if it's using a procedural fallback.
    pub fn get_generated_title(&self, session_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT title FROM generated_titles WHERE session_id = ?1"
        )?;

        let mut rows = stmt.query(params![session_id])?;

        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════════
    // Session Metadata API (continued)
    // ═══════════════════════════════════════════════════════════════════════════════

    /// Enumerate all souls the tower remembers
    ///
    /// Returns every session that has institutional memory attached—any soul with an icon,
    /// notes, navigation history, or other annotations. Pure conversations without metadata
    /// are not included; this filters to souls the tower has been told to remember.
    ///
    /// Useful for:
    /// - Displaying a dashboard of active/tracked work
    /// - Searching for souls by sigil or annotation (filter results in caller)
    /// - Backup/export of the tower's institutional knowledge
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

/// Record that babel generated a haiku title for this session (standalone function)
pub fn set_generated_title(conn: &Connection, session_id: &str, title: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    conn.execute(
        "INSERT INTO generated_titles (session_id, title, generated_at, source)
         VALUES (?1, ?2, ?3, 'haiku')
         ON CONFLICT(session_id) DO UPDATE SET title = ?2, generated_at = ?3",
        params![session_id, title, now],
    ).context("Failed to set generated title")?;
    Ok(())
}

/// Check if babel has a haiku-generated title for this session (standalone function)
pub fn get_generated_title(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT title FROM generated_titles WHERE session_id = ?1"
    )?;

    let mut rows = stmt.query(params![session_id])?;

    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
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

    #[test]
    fn test_generated_title() {
        let db = test_db();
        let session_id = "test-session-haiku";

        // Initially no generated title
        assert!(db.get_generated_title(session_id).unwrap().is_none());

        // Set a haiku-generated title
        db.set_generated_title(session_id, "babel: refactoring auth").unwrap();

        // Now we should get it back
        let title = db.get_generated_title(session_id).unwrap();
        assert_eq!(title, Some("babel: refactoring auth".to_string()));

        // Update the title
        db.set_generated_title(session_id, "babel: auth complete").unwrap();
        let title = db.get_generated_title(session_id).unwrap();
        assert_eq!(title, Some("babel: auth complete".to_string()));
    }
}
