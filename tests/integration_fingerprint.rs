//! Integration Tests for Fingerprint-Based Session Matching
//!
//! These tests require:
//! - Running kitty terminal with agent panes
//! - Active provider sessions in ~/.claude/projects/
//!
//! Run with: cargo test --test integration_fingerprint -- --ignored --nocapture
//!
//! Test scenarios:
//! 1. Scrollback extraction from live terminals
//! 2. JSONL extraction from real session files
//! 3. Cross-matching terminals to sessions
//! 4. Daemon fingerprint index validation
//! 5. End-to-end matching accuracy

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use claude_babel::fingerprint::{
    extract_from_jsonl, extract_from_scrollback, match_fingerprints, MatchConfidence,
    SessionFingerprint,
};
use claude_babel::kitty::get_scrollback;
use claude_babel::utility::agent_discovery::{discover_agent_panes, find_agent_panes};
use claude_babel::utility::claude_storage::{list_projects, list_sessions};

// ═══════════════════════════════════════════════════════════════════════════
// Test Utilities
// ═══════════════════════════════════════════════════════════════════════════

/// Print fingerprint details for debugging
fn print_fingerprint(label: &str, fp: &SessionFingerprint) {
    println!("\n=== {} ===", label);
    println!("  first_prompt: {:?}", fp.first_prompt);
    println!("  recent_prompts: {:?}", fp.recent_prompts);
    println!("  tool_sequence: {:?}", fp.tool_sequence);
    println!("  cwd: {:?}", fp.cwd);
    println!("  session_id: {:?}", fp.session_id);
}

/// Get the most recent N session files by modification time
fn get_recent_session_files(limit: usize) -> Result<Vec<PathBuf>> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    for project_dir in list_projects()? {
        for session_path in list_sessions(&project_dir)? {
            if let Ok(meta) = std::fs::metadata(&session_path) {
                if let Ok(mtime) = meta.modified() {
                    files.push((session_path, mtime));
                }
            }
        }
    }

    files.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(files.into_iter().take(limit).map(|(p, _)| p).collect())
}

// ═══════════════════════════════════════════════════════════════════════════
// Scrollback Extraction Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Test: Extract fingerprints from all live agent panes
#[tokio::test]
#[ignore]
async fn test_live_scrollback_extraction() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Live Scrollback Extraction                            ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    let windows = find_agent_panes().await?;
    println!("Found {} agent panes", windows.len());

    assert!(
        !windows.is_empty(),
        "No agent panes found - start some agent sessions first"
    );

    for window in &windows {
        println!("\n--- Pane {} (title: {}) ---", window.id, window.title);

        let scrollback = get_scrollback(window.id).await?;
        println!(
            "  Scrollback size: {} bytes, {} lines",
            scrollback.len(),
            scrollback.lines().count()
        );

        let fp = extract_from_scrollback(&scrollback);
        print_fingerprint(&format!("Pane {} Fingerprint", window.id), &fp);

        // Validate extraction produced meaningful data
        let has_prompts = fp.first_prompt.is_some() || !fp.recent_prompts.is_empty();
        let has_tools = !fp.tool_sequence.is_empty();
        let has_cwd = fp.cwd.is_some();

        println!("  Extraction quality:");
        println!("    - Has prompts: {}", has_prompts);
        println!("    - Has tools: {}", has_tools);
        println!("    - Has CWD: {}", has_cwd);

        // At least one signal should be present in an active agent session
        assert!(
            has_prompts || has_tools,
            "No prompts or tools extracted from window {} - scrollback may be empty or format changed",
            window.id
        );
    }

    Ok(())
}

/// Test: Validate scrollback patterns against known agent output format
#[tokio::test]
#[ignore]
async fn test_scrollback_pattern_recognition() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Scrollback Pattern Recognition                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    let windows = find_agent_panes().await?;
    if windows.is_empty() {
        println!("SKIP: No agent panes available");
        return Ok(());
    }

    // Take first window
    let window = &windows[0];
    let scrollback = get_scrollback(window.id).await?;

    // Check for expected agent output patterns
    let patterns = [
        ("> ", "User prompt marker"),
        ("● ", "Tool call marker (bullet)"),
        ("• ", "Tool call marker (dot)"),
        ("Bash", "Bash tool usage"),
        ("Read", "Read tool usage"),
        ("Edit", "Edit tool usage"),
        ("cwd:", "CWD indicator"),
    ];

    println!("\nPattern detection in pane {}:", window.id);
    for (pattern, desc) in &patterns {
        let count = scrollback.matches(pattern).count();
        let status = if count > 0 { "✓" } else { "✗" };
        println!("  {} {} ({}x) - {}", status, pattern, count, desc);
    }

    // Extract and validate
    let fp = extract_from_scrollback(&scrollback);

    // Tool sequence should match tool calls in scrollback
    if !fp.tool_sequence.is_empty() {
        println!("\nExtracted tools: {:?}", fp.tool_sequence);
        for tool in &fp.tool_sequence {
            assert!(
                scrollback.contains(&format!("{}(", tool)) || scrollback.contains(tool),
                "Extracted tool '{}' not found in scrollback",
                tool
            );
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// JSONL Extraction Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Test: Extract fingerprints from recent JSONL session files
#[test]
#[ignore]
fn test_jsonl_extraction() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: JSONL Session Extraction                              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    let session_files = get_recent_session_files(10)?;
    println!("Testing {} most recent session files", session_files.len());

    assert!(
        !session_files.is_empty(),
        "No session files found in ~/.claude/projects/"
    );

    for path in &session_files {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        println!("\n--- Session: {} ---", session_id);
        println!("  Path: {}", path.display());

        // Get file metadata
        if let Ok(meta) = std::fs::metadata(path) {
            println!("  Size: {} bytes", meta.len());
        }

        // Extract fingerprint
        match extract_from_jsonl(path) {
            Ok(fp) => {
                print_fingerprint(&format!("Session {}", session_id), &fp);

                // Validate extraction
                let has_prompts = fp.first_prompt.is_some() || !fp.recent_prompts.is_empty();
                let has_tools = !fp.tool_sequence.is_empty();
                let has_cwd = fp.cwd.is_some();

                println!("  Extraction quality:");
                println!("    - Has prompts: {}", has_prompts);
                println!("    - Has tools: {}", has_tools);
                println!("    - Has CWD: {}", has_cwd);

                // Real sessions should have at least prompts
                assert!(
                    has_prompts,
                    "No prompts extracted from session {}",
                    session_id
                );
            }
            Err(e) => {
                println!("  ERROR: Failed to extract: {}", e);
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Cross-Matching Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Test: Match live terminals against JSONL sessions
#[tokio::test]
#[ignore]
async fn test_terminal_to_session_matching() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Terminal → Session Matching                           ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Build fingerprint index from JSONL files
    let session_files = get_recent_session_files(50)?;
    let mut session_fingerprints: HashMap<String, SessionFingerprint> = HashMap::new();

    println!(
        "\nBuilding fingerprint index from {} sessions...",
        session_files.len()
    );
    for path in &session_files {
        if let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(mut fp) = extract_from_jsonl(path) {
                fp.session_id = Some(session_id.to_string());
                session_fingerprints.insert(session_id.to_string(), fp);
            }
        }
    }
    println!("Index contains {} sessions", session_fingerprints.len());

    // Get live agent panes
    let windows = find_agent_panes().await?;
    println!("\nMatching {} live windows...", windows.len());

    if windows.is_empty() {
        println!("SKIP: No agent panes available");
        return Ok(());
    }

    let mut matches_found = 0;
    let mut match_results = Vec::new();

    for window in &windows {
        println!("\n--- Pane {} ---", window.id);
        println!("  Title: {}", window.title);
        println!("  CWD: {}", window.cwd.display());

        let scrollback = get_scrollback(window.id).await?;
        let window_fp = extract_from_scrollback(&scrollback);

        // Find best match
        let mut best_match: Option<(&str, MatchConfidence)> = None;
        let mut all_scores: Vec<(&str, MatchConfidence)> = Vec::new();

        for (session_id, session_fp) in &session_fingerprints {
            let confidence = match_fingerprints(&window_fp, session_fp);
            all_scores.push((session_id.as_str(), confidence));

            if confidence >= MatchConfidence::Medium {
                if let Some((_, best_conf)) = &best_match {
                    if confidence > *best_conf {
                        best_match = Some((session_id.as_str(), confidence));
                    }
                } else {
                    best_match = Some((session_id.as_str(), confidence));
                }
            }
        }

        // Sort and show top matches
        all_scores.sort_by(|a, b| b.1.cmp(&a.1));
        println!("  Top matches:");
        for (session_id, conf) in all_scores.iter().take(3) {
            let marker = if *conf >= MatchConfidence::Medium {
                "→"
            } else {
                " "
            };
            println!("    {} {:?}: {}", marker, conf, session_id);
        }

        if let Some((session_id, confidence)) = best_match {
            println!("  ✓ MATCHED: {} ({:?})", session_id, confidence);
            matches_found += 1;
            match_results.push((window.id, session_id.to_string(), confidence));
        } else {
            println!("  ✗ No confident match found");
        }
    }

    println!("\n═══════════════════════════════════════════════════════════════");
    println!(
        "RESULTS: {}/{} windows matched to sessions",
        matches_found,
        windows.len()
    );
    for (window_id, session_id, confidence) in &match_results {
        println!("  Pane {} → {} ({:?})", window_id, session_id, confidence);
    }

    Ok(())
}

/// Test: Verify matching accuracy against known pairings
#[tokio::test]
#[ignore]
async fn test_matching_accuracy_report() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Matching Accuracy Report                              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Use discover_agent_panes which includes pre-existing tags
    let windows = discover_agent_panes().await?;
    println!("Found {} agent panes via discovery", windows.len());

    // Build fresh fingerprint index
    let session_files = get_recent_session_files(100)?;
    let mut session_fingerprints: HashMap<String, SessionFingerprint> = HashMap::new();

    for path in &session_files {
        if let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(mut fp) = extract_from_jsonl(path) {
                fp.session_id = Some(session_id.to_string());
                session_fingerprints.insert(session_id.to_string(), fp);
            }
        }
    }

    let mut correct = 0;
    let mut incorrect = 0;
    let mut untagged = 0;
    let mut unmatched = 0;

    for window in &windows {
        let scrollback = get_scrollback(window.id()).await?;
        let window_fp = extract_from_scrollback(&scrollback);

        // Find fingerprint match
        let fp_match = session_fingerprints
            .iter()
            .map(|(id, fp)| (id.clone(), match_fingerprints(&window_fp, fp)))
            .filter(|(_, conf)| *conf >= MatchConfidence::Medium)
            .max_by_key(|(_, conf)| *conf)
            .map(|(id, conf)| (id, conf));

        // Compare against existing tag (if any)
        match (&window.session_id, fp_match) {
            (Some(tagged_id), Some((fp_id, conf))) => {
                if tagged_id == &fp_id {
                    println!("  ✓ Pane {}: Correct match ({:?})", window.id(), conf);
                    correct += 1;
                } else {
                    println!("  ✗ Pane {}: Mismatch!", window.id());
                    println!("      Tagged:      {}", tagged_id);
                    println!("      Fingerprint: {} ({:?})", fp_id, conf);
                    incorrect += 1;
                }
            }
            (Some(tagged_id), None) => {
                println!("  ? Pane {}: Tagged but no FP match", window.id());
                println!("      Tagged: {}", tagged_id);
                unmatched += 1;
            }
            (None, Some((fp_id, conf))) => {
                println!("  + Pane {}: Untagged, FP suggests {}", window.id(), fp_id);
                println!("      Confidence: {:?}", conf);
                untagged += 1;
            }
            (None, None) => {
                println!("  - Pane {}: No tag, no FP match", window.id());
                unmatched += 1;
            }
        }
    }

    println!("\n═══════════════════════════════════════════════════════════════");
    println!("ACCURACY REPORT:");
    println!("  Correct matches:    {}", correct);
    println!("  Incorrect matches:  {}", incorrect);
    println!("  Newly matchable:    {}", untagged);
    println!("  Unmatched:          {}", unmatched);

    if correct + incorrect > 0 {
        let accuracy = (correct as f64 / (correct + incorrect) as f64) * 100.0;
        println!("  Accuracy:           {:.1}%", accuracy);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Daemon Integration Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Test: Verify daemon fingerprint index matches our expectations
#[test]
#[ignore]
fn test_daemon_fingerprint_index() -> Result<()> {
    use claude_babel::utility::ipc::{send_request_sync, Request, Response};

    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Daemon Fingerprint Index                              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Check daemon is running
    match send_request_sync(&Request::Ping) {
        Ok(Response::Pong { uptime_secs }) => {
            println!("Daemon running for {} seconds", uptime_secs);
        }
        _ => {
            println!("SKIP: Daemon not running (start with 'babel daemon')");
            return Ok(());
        }
    }

    // Get windows from daemon
    let response = send_request_sync(&Request::List)?;
    if let Response::Windows { windows } = response {
        println!("\nDaemon reports {} windows:", windows.len());

        for window in &windows {
            let session_status = match &window.session_id {
                Some(id) => format!("→ {}", id),
                None => "unmatched".to_string(),
            };
            println!(
                "  Pane {}: {} [{}]",
                window.id(),
                window.title,
                session_status
            );
        }

        // Force refresh and compare
        println!("\nForcing refresh...");
        send_request_sync(&Request::Refresh)?;

        let after_refresh = send_request_sync(&Request::List)?;
        if let Response::Windows {
            windows: after_windows,
        } = after_refresh
        {
            let matched_before = windows.iter().filter(|w| w.session_id.is_some()).count();
            let matched_after = after_windows
                .iter()
                .filter(|w| w.session_id.is_some())
                .count();

            println!("Windows matched before refresh: {}", matched_before);
            println!("Windows matched after refresh:  {}", matched_after);

            if matched_after > matched_before {
                println!(
                    "✓ Fingerprint matching found {} new associations",
                    matched_after - matched_before
                );
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// Data Quality Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Test: Analyze fingerprint data quality across all sources
#[tokio::test]
#[ignore]
async fn test_fingerprint_data_quality() -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Test: Fingerprint Data Quality Analysis                     ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Analyze JSONL fingerprints
    let session_files = get_recent_session_files(50)?;
    let mut jsonl_stats = DataQualityStats::default();

    for path in &session_files {
        if let Ok(fp) = extract_from_jsonl(path) {
            jsonl_stats.record(&fp);
        }
    }

    println!(
        "\n=== JSONL Extraction Quality (n={}) ===",
        jsonl_stats.total
    );
    jsonl_stats.print();

    // Analyze scrollback fingerprints
    let windows = find_agent_panes().await.unwrap_or_default();
    let mut scrollback_stats = DataQualityStats::default();

    for window in &windows {
        if let Ok(scrollback) = get_scrollback(window.id).await {
            let fp = extract_from_scrollback(&scrollback);
            scrollback_stats.record(&fp);
        }
    }

    println!(
        "\n=== Scrollback Extraction Quality (n={}) ===",
        scrollback_stats.total
    );
    scrollback_stats.print();

    // Recommendations
    println!("\n=== Recommendations ===");
    if jsonl_stats.first_prompt_pct() < 80.0 {
        println!(
            "  ⚠ JSONL first_prompt extraction low ({:.0}%) - check parser",
            jsonl_stats.first_prompt_pct()
        );
    }
    if scrollback_stats.tool_pct() < 50.0 && scrollback_stats.total > 0 {
        println!(
            "  ⚠ Scrollback tool extraction low ({:.0}%) - patterns may have changed",
            scrollback_stats.tool_pct()
        );
    }
    if scrollback_stats.first_prompt_pct() < jsonl_stats.first_prompt_pct() * 0.5 {
        println!("  ⚠ Scrollback prompt extraction much worse than JSONL - check patterns");
    }

    Ok(())
}

#[derive(Default)]
struct DataQualityStats {
    total: usize,
    has_first_prompt: usize,
    has_recent_prompts: usize,
    has_tools: usize,
    has_cwd: usize,
    avg_prompt_len: f64,
    avg_tool_count: f64,
}

impl DataQualityStats {
    fn record(&mut self, fp: &SessionFingerprint) {
        self.total += 1;
        if fp.first_prompt.is_some() {
            self.has_first_prompt += 1;
        }
        if !fp.recent_prompts.is_empty() {
            self.has_recent_prompts += 1;
        }
        if !fp.tool_sequence.is_empty() {
            self.has_tools += 1;
        }
        if fp.cwd.is_some() {
            self.has_cwd += 1;
        }

        // Running averages
        let n = self.total as f64;
        if let Some(ref prompt) = fp.first_prompt {
            self.avg_prompt_len = self.avg_prompt_len * ((n - 1.0) / n) + prompt.len() as f64 / n;
        }
        self.avg_tool_count =
            self.avg_tool_count * ((n - 1.0) / n) + fp.tool_sequence.len() as f64 / n;
    }

    fn first_prompt_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.has_first_prompt as f64 / self.total as f64 * 100.0
        }
    }

    fn tool_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.has_tools as f64 / self.total as f64 * 100.0
        }
    }

    fn print(&self) {
        if self.total == 0 {
            println!("  No samples collected");
            return;
        }
        println!(
            "  has_first_prompt:  {:>3}/{} ({:.0}%)",
            self.has_first_prompt,
            self.total,
            self.first_prompt_pct()
        );
        println!(
            "  has_recent_prompts:{:>3}/{} ({:.0}%)",
            self.has_recent_prompts,
            self.total,
            self.has_recent_prompts as f64 / self.total as f64 * 100.0
        );
        println!(
            "  has_tools:         {:>3}/{} ({:.0}%)",
            self.has_tools,
            self.total,
            self.tool_pct()
        );
        println!(
            "  has_cwd:           {:>3}/{} ({:.0}%)",
            self.has_cwd,
            self.total,
            self.has_cwd as f64 / self.total as f64 * 100.0
        );
        println!("  avg_prompt_len:    {:.0} chars", self.avg_prompt_len);
        println!("  avg_tool_count:    {:.1} tools", self.avg_tool_count);
    }
}
