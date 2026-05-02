//! Test Claude storage parsing with real data

use anyhow::Result;
use babel::utility::claude_storage;

fn main() -> Result<()> {
    println!("=== Testing Claude Storage Parser ===\n");

    // Test 1: List projects
    println!("1. Listing projects...");
    let projects = claude_storage::list_projects()?;
    println!("   Found {} projects", projects.len());
    for (i, project) in projects.iter().take(5).enumerate() {
        println!("   [{}] {}", i + 1, project.display());
    }
    println!();

    // Test 2: Get recent sessions
    println!("2. Getting recent sessions (limit: 5)...");
    let recent = claude_storage::get_recent_sessions(5)?;
    println!("   Found {} recent sessions", recent.len());
    for (i, session) in recent.iter().enumerate() {
        println!("   [{}] Session: {}", i + 1, session.session_id);
        println!("       Project: {}", session.project.display());
        println!("       Summaries: {}", session.summaries.len());
        if let Some(first_summary) = session.summaries.first() {
            println!("       Latest: {}", first_summary.summary);
        }
        println!();
    }

    // Test 3: Find session by summary
    if let Some(first_session) = recent.first() {
        if let Some(summary) = first_session.summaries.first() {
            let query = &summary.summary[..20.min(summary.summary.len())];
            println!("3. Finding session by summary query: '{}'...", query);
            if let Some(found) = claude_storage::find_session_by_summary(query)? {
                println!("   Found: {}", found.session_id);
                println!("   Project: {}", found.project.display());
            } else {
                println!("   Not found (unexpected!)");
            }
        }
    }

    Ok(())
}
