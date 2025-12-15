# Discovery Module Refactoring Audit

**Date**: 2025-12-15
**Module**: `src/discovery.rs`
**Focus**: Code quality, refactoring opportunities, potential bugs

---

## Executive Summary

The `discovery.rs` module is well-structured and reasonably clean, but contains several opportunities for improvement:

- **3 High Priority** refactoring opportunities
- **4 Medium Priority** improvements
- **2 Low Priority** polish items
- **1 Potential Bug** (logic inconsistency)

The module's core responsibility (matching kitty windows to Claude sessions) is clear, but some functions violate single responsibility principle and contain duplicated logic.

---

## High Priority Issues

### 1. **`load_session_by_id` is Inefficient** (Lines 213-230)

**Problem**: Nested loops scan ALL projects × ALL sessions to find ONE session ID. This is O(n×m) when it should be O(1) or O(n).

```rust
// Current: Scans EVERYTHING
for project_dir in list_projects()? {
    for session_path in list_sessions(&project_dir)? {
        if let Some(file_stem) = session_path.file_stem().and_then(|s| s.to_str()) {
            if file_stem == session_id {
                let info = get_session_info(&session_path)?;
                return Ok(Some(info));
            }
        }
    }
}
```

**Impact**:
- Called in hot path (daemon refresh loop via `enrich_window`)
- If you have 10 projects × 20 sessions each = 200 filesystem checks to find 1 file
- No early exit or caching

**Solution**:
```rust
// Better: Direct construction of path
fn load_session_by_id_direct(session_id: &str) -> Result<Option<SessionInfo>> {
    // Try to extract project path from cached windows or daemon state
    // Or: maintain a reverse index (session_id → project_path) in daemon

    // Fallback: still need to search, but at least log it
    tracing::warn!("load_session_by_id doing full scan for {}", session_id);
    // ... existing scan logic
}
```

**Recommendation**:
- Add session_id → project mapping to `DaemonState`
- Or: Store project path in the user_var tag alongside session_id
- Or: At minimum, add logging to detect how often this scans

---

### 2. **`enrich_window` and `match_window_to_session` Have Duplicated Logic** (Lines 102-169)

**Problem**: Both functions do nearly identical work:
1. Check if session_id exists in user_vars → load via `load_session_by_id`
2. Extract summary from title → `find_session_by_summary`
3. Tag the window

The only difference: one mutates a `&mut ClaudeWindow`, the other takes `&KittyWindow`.

```rust
// enrich_window (lines 102-130)
if let Some(ref session_id) = window.session_id {
    if let Some(info) = load_session_by_id(session_id)? {
        window.session_info = Some(info);
        return Ok(());
    }
}
let summary = extract_summary_from_title(&window.title);
// ... find and tag

// match_window_to_session (lines 141-169) - SAME LOGIC
if let Some(session_id) = window.user_vars.get("babel_session_id") {
    if let Some(info) = load_session_by_id(session_id)? {
        return Ok(Some(info));
    }
}
let summary = extract_summary_from_title(&window.title);
// ... find and tag
```

**Impact**:
- Code duplication = double maintenance burden
- Easy to introduce bugs when updating one but not the other
- Unclear which function to call in different contexts

**Solution**: Extract shared logic into a helper:

```rust
/// Core matching logic - shared between enrich and match operations
fn resolve_session_info(
    session_id: Option<&str>,
    title: &str,
    kitty_id: u64,
) -> Result<Option<SessionInfo>> {
    // Try tagged session first
    if let Some(session_id) = session_id {
        if !session_id.is_empty() {
            if let Some(info) = load_session_by_id(session_id)? {
                return Ok(Some(info));
            }
        }
    }

    // Extract summary from title
    let summary = extract_summary_from_title(title);
    if summary.is_empty() {
        return Ok(None);
    }

    // Find matching session
    let session_info = find_session_by_summary(&summary)?;

    // Tag window if matched
    if let Some(ref info) = session_info {
        let _ = tag_window(kitty_id, &info.session_id);
    }

    Ok(session_info)
}
```

Then simplify callers:
```rust
pub fn enrich_window(window: &mut ClaudeWindow) -> Result<()> {
    if window.session_info.is_some() {
        return Ok(());
    }

    let info = resolve_session_info(
        window.session_id.as_deref(),
        &window.title,
        window.kitty_id,
    )?;

    window.session_info = info;
    Ok(())
}

pub fn match_window_to_session(window: &KittyWindow) -> Result<Option<SessionInfo>> {
    resolve_session_info(
        window.user_vars.get("babel_session_id").map(|s| s.as_str()),
        &window.title,
        window.id,
    )
}
```

---

### 3. **`find_window_by_session` is Broken** (Lines 183-191)

**Problem**: Function searches for `window.session_info.session_id == session_id`, but `discover_claude_windows()` returns windows with `session_info = None` (lazy loading).

```rust
pub fn find_window_by_session(session_id: &str) -> Result<Option<ClaudeWindow>> {
    let windows = discover_claude_windows()?;
    Ok(windows.into_iter().find(|w| {
        w.session_info              // ← This is ALWAYS None!
            .as_ref()
            .map(|s| s.session_id.as_str())
            == Some(session_id)
    }))
}
```

**Impact**:
- Function **never works** - always returns `None`
- Dead code or callers are working around it

**Grep Check**:
```bash
# Is this function even used?
rg "find_window_by_session" --type rust
```

**Solution**:
```rust
pub fn find_window_by_session(session_id: &str) -> Result<Option<ClaudeWindow>> {
    let windows = discover_claude_windows()?;

    // First try: match by session_id tag (fast)
    if let Some(window) = windows.iter().find(|w| {
        w.session_id.as_deref() == Some(session_id)
    }) {
        return Ok(Some(window.clone()));
    }

    // Second try: enrich all and search session_info (slow fallback)
    for mut window in windows {
        enrich_window(&mut window)?;
        if window.session_info.as_ref().map(|s| s.session_id.as_str()) == Some(session_id) {
            return Ok(Some(window));
        }
    }

    Ok(None)
}
```

**Better Alternative**: Don't search - lookup directly from daemon state:
```rust
// In daemon.rs - add this to DaemonState
pub fn find_window_by_session(&self, session_id: &str) -> Option<&ClaudeWindow> {
    self.windows.values().find(|w| {
        w.session_id.as_deref() == Some(session_id)
    })
}
```

---

## Medium Priority Issues

### 4. **`enrich_window` Swallows Critical Errors** (Lines 102-130)

**Problem**: Uses `let _ = tag_window(...)` to silently ignore tagging failures.

```rust
// Line 124
let _ = tag_window(window.kitty_id, &info.session_id);
```

**Impact**:
- If tagging fails, window won't be cached for future lookups
- Will re-scan filesystem every time instead of O(1) lookup
- No visibility into why tagging failed (permissions? socket issues?)

**Solution**:
```rust
// Log the error but don't abort the operation
if let Err(e) = tag_window(window.kitty_id, &info.session_id) {
    tracing::warn!(
        window_id = window.kitty_id,
        session_id = %info.session_id,
        error = %e,
        "Failed to tag window - will re-scan next time"
    );
}
```

---

### 5. **`discover_claude_windows` Mixes Concerns** (Lines 58-96)

**Problem**: Function does 3 things:
1. Find Claude windows (kitty API call)
2. Get workspace mappings (wmctrl call)
3. Extract session tags (parsing kitty output)

**Impact**:
- Hard to test each concern independently
- Workspace logic could be reused elsewhere but it's buried here

**Solution**: Extract workspace mapping to a helper:

```rust
/// Map kitty windows to their XFCE workspaces
fn map_windows_to_workspaces(windows: &[KittyWindow]) -> HashMap<u64, i32> {
    use crate::kitty::get_all_workspaces;

    let workspace_map = get_all_workspaces();
    windows.iter()
        .filter_map(|w| workspace_map.get(&w.platform_window_id).map(|&ws| (w.id, ws)))
        .collect()
}
```

Then simplify the main function:
```rust
pub fn discover_claude_windows() -> Result<Vec<ClaudeWindow>> {
    let kitty_windows = find_claude_windows()
        .context("Failed to find claude windows")?;

    let workspace_map = map_windows_to_workspaces(&kitty_windows);

    let discovered = kitty_windows.into_iter()
        .map(|window| build_claude_window(window, &workspace_map))
        .collect();

    Ok(discovered)
}

fn build_claude_window(window: KittyWindow, workspaces: &HashMap<u64, i32>) -> ClaudeWindow {
    let session_id = window.user_vars
        .get("babel_session_id")
        .filter(|s| !s.is_empty())
        .cloned();

    let workspace = workspaces.get(&window.id).copied();

    ClaudeWindow {
        kitty_id: window.id,
        title: window.title,
        session_id,
        session_info: None,
        cwd: window.cwd,
        is_focused: window.is_focused,
        os_window_id: window.os_window_id,
        platform_window_id: window.platform_window_id,
        workspace,
        fingerprint: None,
        match_confidence: None,
    }
}
```

---

### 6. **Inconsistent Error Handling in `match_window_to_session`** (Lines 141-169)

**Problem**: Function ignores `load_session_by_id` errors but propagates `find_session_by_summary` errors.

```rust
// Line 147: Silently ignores load errors
if let Some(info) = load_session_by_id(session_id)? {
    return Ok(Some(info));
}
// Falls through on error - no logging

// Line 161: Propagates find errors
let session_info = find_session_by_summary(&summary)?;
```

**Impact**:
- Inconsistent behavior - some errors bubble up, others don't
- Hard to debug when tagged sessions can't be loaded

**Solution**: Log errors consistently:
```rust
if let Some(session_id) = window.user_vars.get("babel_session_id") {
    if !session_id.is_empty() {
        match load_session_by_id(session_id) {
            Ok(Some(info)) => return Ok(Some(info)),
            Ok(None) => {
                tracing::debug!(
                    window_id = window.id,
                    session_id = %session_id,
                    "Tagged session not found - will re-match"
                );
            }
            Err(e) => {
                tracing::warn!(
                    window_id = window.id,
                    session_id = %session_id,
                    error = %e,
                    "Failed to load tagged session - will re-match"
                );
            }
        }
    }
}
```

---

### 7. **Missing Validation in `tag_window`** (Lines 175-178)

**Problem**: Function doesn't validate session_id format or emptiness.

```rust
pub fn tag_window(kitty_id: u64, session_id: &str) -> Result<()> {
    set_user_var(kitty_id, "babel_session_id", session_id)
        .context("Failed to tag window with session ID")
}
```

**Impact**:
- Could tag windows with empty strings or malformed UUIDs
- No logging to track tagging operations (useful for debugging)

**Solution**:
```rust
pub fn tag_window(kitty_id: u64, session_id: &str) -> Result<()> {
    // Validate session_id
    if session_id.is_empty() {
        anyhow::bail!("Cannot tag window with empty session_id");
    }

    // UUID format check (loose - just verify it's not gibberish)
    if session_id.len() < 8 || session_id.contains(char::is_whitespace) {
        tracing::warn!(
            window_id = kitty_id,
            session_id = %session_id,
            "Tagging window with suspicious session_id"
        );
    }

    tracing::debug!(
        window_id = kitty_id,
        session_id = %session_id,
        "Tagging window"
    );

    set_user_var(kitty_id, "babel_session_id", session_id)
        .context("Failed to tag window with session ID")
}
```

---

## Low Priority Polish

### 8. **`extract_summary_from_title` Could Be More Robust** (Lines 201-207)

**Current**: Only strips "✳ " prefix.

**Issue**: What if Claude changes the prefix format? Or adds emoji variations?

**Suggestion**:
```rust
fn extract_summary_from_title(title: &str) -> String {
    // Handle multiple known prefixes
    let prefixes = ["✳ ", "● ", "▸ "];  // Add as discovered

    for prefix in &prefixes {
        if let Some(summary) = title.strip_prefix(prefix) {
            return summary.trim().to_string();
        }
    }

    // Fallback: check if title looks like a session (heuristic)
    // Active sessions rarely contain ":" or "~/" (those are shell prompts)
    if !title.contains(':') && !title.contains("~/") {
        return title.trim().to_string();
    }

    String::new()
}
```

---

### 9. **Module Documentation Could Link to Related Modules** (Lines 1-21)

**Current**: Explains matching strategy well, but doesn't point to:
- `claude_storage.rs` - where `find_session_by_summary` lives
- `fingerprint.rs` - mentioned in doc but not linked
- `daemon.rs` - where `enrich_window` is heavily used

**Suggestion**: Add cross-references:
```rust
//! See also:
//! - [`claude_storage`] - Session file parsing and search
//! - [`fingerprint`] - Advanced matching via scrollback analysis
//! - [`daemon`] - Background service that calls discovery APIs
```

---

## Potential Edge Cases

### 10. **What Happens When Multiple Windows Match the Same Session?**

**Scenario**: User spawns two Claude sessions with identical titles (parallel work, testing, etc.)

**Current Behavior**:
- Both windows get matched to the SAME session_id
- Both get tagged with the same session
- No way to disambiguate

**Impact**: Probably rare, but could cause confusion in:
- `find_window_by_session` - which window to return?
- Daemon state - which window is "authoritative"?

**Recommendation**:
- Document this as expected behavior (multiple windows CAN share a session)
- Or: Add timestamp/PID to user_vars to track "primary" vs "clones"

---

## Naming Clarity

### 11. **`enrich_window` vs `match_window_to_session` - Unclear When to Use Which**

**Problem**: Both functions match windows to sessions, but:
- `enrich_window`: Takes `&mut ClaudeWindow`, modifies in place
- `match_window_to_session`: Takes `&KittyWindow`, returns `Option<SessionInfo>`

**When to use which?**
- Daemon uses `enrich_window` (has `ClaudeWindow`)
- CLI uses... both? Neither consistently?

**Suggestion**: Rename to clarify intent:
```rust
// Current
pub fn enrich_window(window: &mut ClaudeWindow) -> Result<()>
pub fn match_window_to_session(window: &KittyWindow) -> Result<Option<SessionInfo>>

// Clearer
pub fn enrich_with_session_info(window: &mut ClaudeWindow) -> Result<()>
pub fn lookup_session_for_window(window: &KittyWindow) -> Result<Option<SessionInfo>>
```

---

## Testing Gaps

### 12. **Only One Test Exists** (Lines 232-245)

**Current**: Single test for `extract_summary_from_title`.

**Missing Tests**:
- `tag_window` - mock kitty calls, verify user_var is set
- `load_session_by_id` - mock filesystem, test caching
- `enrich_window` - integration test with fixture session files
- `find_window_by_session` - test the bug (should fail currently)

**Suggestion**: Add test fixtures:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mock_window(title: &str, session_id: Option<String>) -> ClaudeWindow {
        ClaudeWindow {
            kitty_id: 1,
            title: title.to_string(),
            session_id,
            session_info: None,
            cwd: PathBuf::from("/tmp"),
            is_focused: false,
            os_window_id: 1,
            platform_window_id: 1,
            workspace: Some(0),
            fingerprint: None,
            match_confidence: None,
        }
    }

    #[test]
    fn test_enrich_window_skips_if_already_enriched() {
        let mut window = mock_window("✳ Test", Some("abc123".into()));
        window.session_info = Some(/* mock SessionInfo */);

        // Should return immediately without filesystem access
        assert!(enrich_window(&mut window).is_ok());
    }

    #[test]
    fn test_find_window_by_session_actually_works() {
        // This should FAIL with current implementation
        // Once fixed, it should pass
    }
}
```

---

## Summary of Recommendations

| Priority | Issue | Action | Impact |
|----------|-------|--------|--------|
| **HIGH** | `load_session_by_id` inefficiency | Add daemon-level caching or direct path construction | 🚀 Performance - avoid O(n×m) scans |
| **HIGH** | Duplicated logic in `enrich_window` / `match_window_to_session` | Extract `resolve_session_info` helper | 🧹 DRY, easier maintenance |
| **HIGH** | `find_window_by_session` is broken | Fix search logic or remove dead code | 🐛 Bug fix |
| **MEDIUM** | `enrich_window` swallows errors | Add warning logs for tagging failures | 🔍 Debuggability |
| **MEDIUM** | `discover_claude_windows` mixes concerns | Extract `build_claude_window` and `map_windows_to_workspaces` | 🧩 Testability |
| **MEDIUM** | Inconsistent error handling | Standardize logging for all errors | 🔍 Debuggability |
| **MEDIUM** | `tag_window` lacks validation | Add session_id validation and logging | 🛡️ Robustness |
| **LOW** | `extract_summary_from_title` hardcoded | Support multiple prefixes, add heuristics | 🛡️ Future-proofing |
| **LOW** | Missing module cross-references | Add doc links to related modules | 📚 Navigation |

---

## Refactoring Strategy

**Recommended Order**:

1. **Fix bug first**: `find_window_by_session` (5 min)
2. **Add logging**: `enrich_window`, `match_window_to_session`, `tag_window` (15 min)
3. **Extract helper**: `resolve_session_info` to eliminate duplication (30 min)
4. **Optimize hot path**: `load_session_by_id` caching in daemon (45 min)
5. **Polish**: Validation, naming, tests (1-2 hours)

**Total Estimate**: 2-3 hours for complete refactor

---

## Open Questions

1. **Is `find_window_by_session` actually used?** Grep shows no callers - possibly dead code.
2. **Should we cache `load_session_by_id` results?** Daemon already has `summary_index`, why not `session_index`?
3. **Why do we need both `ClaudeWindow` and `KittyWindow`?** Could we unify them or make conversion explicit?

---

## Next Steps

- [ ] Validate findings with integration tests
- [ ] Check if `find_window_by_session` has callers (may be dead code)
- [ ] Propose API changes to daemon for session_id → project mapping
- [ ] Write tests for edge cases (multiple windows, missing sessions, etc.)
