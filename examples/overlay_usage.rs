//! Example: Using the overlay database to enrich agent sessions
//!
//! This demonstrates how to use the OverlayDb to add user metadata
//! to Claude conversations without modifying Claude's native storage.

use anyhow::Result;
use claude_babel::babel_storage::{BabelStorage, SessionMetadata};

fn main() -> Result<()> {
    // Open the database (creates if needed)
    let db = BabelStorage::open()?;

    // Example session ID (in real use, this would come from Claude)
    let session_id = "f7e4a9c0-1234-5678-90ab-cdef12345678";

    // Set a custom icon for visual identification
    db.set_icon(session_id, "🔥")?;

    // Add navigation chapters as user explores the conversation
    db.add_chapter(session_id, "Initial planning")?;
    db.add_chapter(session_id, "Implementation")?;
    db.add_chapter(session_id, "Testing & debugging")?;

    // Add personal notes
    db.set_notes(
        session_id,
        "Refactoring auth module - needs review before merge",
    )?;

    // Mark as read after reviewing
    db.mark_read(session_id)?;

    // Retrieve all metadata for display
    if let Some(meta) = db.get_metadata(session_id)? {
        println!("Session: {}", meta.session_id);
        println!("Icon: {}", meta.icon.unwrap_or_default());
        println!("Read: {}", meta.is_read);
        println!("Chapters visited:");
        for (i, chapter) in meta.chapter_history.iter().enumerate() {
            println!("  {}. {}", i + 1, chapter);
        }
        if let Some(notes) = meta.notes {
            println!("Notes: {}", notes);
        }
    }

    // List all enriched sessions
    println!("\nAll sessions with metadata:");
    for meta in db.list_all()? {
        println!(
            "  {} {} [{}]",
            meta.icon.unwrap_or_default(),
            meta.session_id,
            if meta.is_read { "read" } else { "unread" }
        );
    }

    Ok(())
}
