# claude-babel Daemon Refactoring Audit

**Date**: 2025-12-15
**Scope**: `src/daemon.rs` - Refactoring opportunities and code quality improvements
**Related**: See `AUDIT-BUGS.md` for bug-specific issues

---

## Executive Summary

The daemon.rs file (1343 lines) is the heart of claude-babel, managing window tracking, session matching, event publishing, and IPC. While functionally sound, it suffers from:

1. **Monolithic functions** - Several functions exceed 100 lines doing multiple things
2. **Lock coupling** - Complex lock acquisition patterns make concurrency hard to reason about
3. **Duplicated patterns** - Similar error handling repeated across IPC handlers
4. **Implicit state transitions** - Window lifecycle not clearly modeled
5. **Missing abstractions** - Event publishing, fingerprinting, and state management could be extracted

**Key Metric**: The `process_request` function alone is 410 lines (64-90% match arms).

---

## Refactoring Opportunities by Priority

### 🔴 HIGH PRIORITY - Complexity Reduction

#### 1. Extract `process_request` Match Arms into Handler Methods

**Location**: Lines 928-1342
**Issue**: The `process_request` function is a massive 410-line match statement handling 16 different request types. Each arm contains business logic that should be tested independently.

**Current Pattern**:
```rust
async fn process_request(
    request: Request,
    state: &Arc<RwLock<DaemonState>>,
    summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Response {
    match request {
        Request::List => {
            let s = state.read().await;
            let windows: Vec<ClaudeWindow> = s.windows.values().cloned().collect();
            Response::Windows { windows }
        }
        Request::ListWithFingerprints => {
            // 30 lines of complex logic
            // ...
        }
        Request::WSetSave { name } => {
            // 28 lines of complex logic
            // ...
        }
        // ... 13 more match arms
    }
}
```

**Proposed Refactoring**:
```rust
// Each handler is a separate, testable function
async fn handle_list(state: &DaemonState) -> Response {
    let windows: Vec<ClaudeWindow> = state.windows.values().cloned().collect();
    Response::Windows { windows }
}

async fn handle_list_with_fingerprints(state: &Arc<RwLock<DaemonState>>) -> Response {
    // Isolated business logic with clear lock acquisition
    let mut windows = {
        let s = state.read().await;
        s.windows.values().cloned().collect()
    };

    // Expensive I/O outside lock
    for win in &mut windows {
        if win.fingerprint.is_none() {
            if let Ok(scrollback) = get_scrollback(win.kitty_id) {
                win.fingerprint = Some(extract_from_scrollback(&scrollback));
            }
        }
        if win.session_info.is_none() {
            let _ = enrich_window(win);
        }
    }

    Response::Windows { windows }
}

async fn handle_wset_save(name: Option<String>, state: &Arc<RwLock<DaemonState>>) -> Response {
    // ... extracted logic
}

// Dispatcher becomes readable
async fn process_request(
    request: Request,
    state: &Arc<RwLock<DaemonState>>,
    summarizer: &Arc<crate::summarizer::WorkspaceSummarizer>,
) -> Response {
    match request {
        Request::List => {
            let s = state.read().await;
            handle_list(&s).await
        }
        Request::ListWithFingerprints => handle_list_with_fingerprints(state).await,
        Request::WSetSave { name } => handle_wset_save(name, state).await,
        // ... delegate to handlers
    }
}
```

**Benefits**:
- Each handler is independently testable
- Clear lock acquisition patterns per handler
- Easier to trace which handlers hold locks for how long
- Functions under 50 lines each
- Better error handling isolation

**Lines Affected**: 928-1342 (414 lines → ~20 handler functions)

---

#### 2. Extract Window Lifecycle Management into `WindowLifecycle` Struct

**Location**: Lines 127-289 (`refresh_windows` method)
**Issue**: The 163-line `refresh_windows` method handles:
- Window discovery
- State preservation
- Session matching
- Event emission
- Fingerprint cleanup
- State tracking
- Workspace change detection

This violates single responsibility and makes testing impossible.

**Current Symptoms**:
- Complex nested logic with 4 levels of indentation
- Multiple mutable state collections being juggled
- Mixed concerns: I/O, state management, event publishing
- Hard to understand window lifecycle transitions

**Proposed Refactoring**:
```rust
/// Manages the lifecycle of a single window
struct WindowLifecycle {
    kitty_id: u64,
    old_state: Option<ClaudeWindow>,
    new_kitty_data: KittyWindow,
}

impl WindowLifecycle {
    /// Determine what changed and what actions to take
    fn plan_transition(&self, summary_index: &[SummaryEntry]) -> WindowTransition {
        match (&self.old_state, &self.new_kitty_data) {
            (None, new) => WindowTransition::Added {
                should_match: true,
                initial_session_id: new.user_vars.get("babel_session_id")
                    .filter(|id| !id.starts_with("agent-"))
                    .cloned(),
            },
            (Some(old), new) if old.title != new.title => WindowTransition::TitleChanged {
                old_title: old.title.clone(),
                new_title: new.title.clone(),
                needs_rematch: true,
            },
            (Some(old), new) if old.is_focused != new.is_focused => WindowTransition::FocusChanged {
                gained_focus: new.is_focused,
            },
            _ => WindowTransition::Updated,
        }
    }

    /// Apply the transition, updating state and emitting events
    fn execute_transition(
        &self,
        transition: WindowTransition,
        state: &mut DaemonState,
    ) -> Vec<BabelEvent> {
        // Clear event emission logic
        // Returns events to publish (separation of concerns)
    }
}

enum WindowTransition {
    Added { should_match: bool, initial_session_id: Option<String> },
    TitleChanged { old_title: String, new_title: String, needs_rematch: bool },
    FocusChanged { gained_focus: bool },
    Removed,
    Updated, // Metadata changed but nothing interesting
}

impl DaemonState {
    pub fn refresh_windows(&mut self) -> Result<Vec<i32>> {
        let kitty_windows = find_claude_windows()?;
        let workspaces = get_all_workspaces();

        // Build lifecycle plans
        let mut lifecycles: Vec<WindowLifecycle> = /* ... */;
        let mut events_to_publish = Vec::new();
        let mut changed_workspaces = HashSet::new();

        for lifecycle in lifecycles {
            let transition = lifecycle.plan_transition(&self.summary_index);
            let events = lifecycle.execute_transition(transition, self);
            events_to_publish.extend(events);
        }

        // Bulk publish events
        for event in events_to_publish {
            self.event_publisher.publish(event);
        }

        Ok(changed_workspaces.into_iter().collect())
    }
}
```

**Benefits**:
- Clear separation: planning vs. execution
- Each lifecycle transition is explicitly named
- Easy to add new transition types
- Testable in isolation (mock DaemonState)
- Event publishing becomes a batch operation

**Lines Affected**: 127-289 (163 lines → ~80 lines in refresh_windows + 100 lines in new structs)

---

#### 3. Extract Fingerprint Matching into `FingerprintMatcher` Service

**Location**: Lines 308-417 (`get_windows_needing_fingerprints`, `fingerprint_match_window_id`, `apply_fingerprint_result`)
**Issue**: Fingerprint matching logic is split across multiple methods with complex lock coordination. The pattern "read state → I/O → write state" is used in 3 places with slight variations.

**Proposed Refactoring**:
```rust
/// Stateless service for fingerprint-based session matching
struct FingerprintMatcher {
    /// Cached index for fast matching
    index: HashMap<String, SessionFingerprint>,
}

impl FingerprintMatcher {
    /// Match a single window to a session
    /// Does expensive I/O, returns result to be applied later
    fn match_window(&self, window_id: u64) -> Result<MatchResult> {
        let scrollback = get_scrollback(window_id)?;
        let window_fp = extract_from_scrollback(&scrollback);

        let best_match = self.find_best_match(&window_fp)?;

        Ok(MatchResult {
            window_id,
            session_id: best_match.session_id,
            confidence: best_match.confidence,
            fingerprint: window_fp,
        })
    }

    /// Internal: find best match in index
    fn find_best_match(&self, window_fp: &SessionFingerprint) -> Result<Match> {
        // Clean extraction of matching logic
        // Currently buried in fingerprint_match_window_id
    }

    /// Batch match multiple windows (for parallelism)
    async fn match_batch(&self, window_ids: Vec<u64>) -> Vec<Result<MatchResult>> {
        futures::future::join_all(
            window_ids.into_iter().map(|id| self.match_window(id))
        ).await
    }
}

struct MatchResult {
    window_id: u64,
    session_id: String,
    confidence: MatchConfidence,
    fingerprint: SessionFingerprint,
}

impl DaemonState {
    /// Apply fingerprint match results (quick, lock-safe)
    fn apply_match_results(&mut self, results: Vec<MatchResult>) {
        for result in results {
            // Tag window
            let _ = crate::discovery::tag_window(result.window_id, &result.session_id);

            // Cache fingerprint
            self.cache_fingerprint(result.window_id, result.fingerprint.clone());

            // Update window state
            if let Some(window) = self.windows.get_mut(&result.window_id) {
                window.session_id = Some(result.session_id);
                window.match_confidence = Some(result.confidence);
                window.fingerprint = Some(result.fingerprint);
            }
        }
    }
}
```

**Usage in main loop**:
```rust
// Before: interleaved lock/unlock/lock in main loop
// After: clear phases
let (needs_matching, matcher) = {
    let s = state.read().await;
    let needs = s.get_windows_needing_fingerprints();
    let matcher = FingerprintMatcher { index: s.fingerprint_index.clone() };
    (needs, matcher)
};

// Parallel matching without any locks
let results = matcher.match_batch(needs_matching).await;

// Apply results with brief write lock
{
    let mut s = state.write().await;
    s.apply_match_results(results.into_iter().filter_map(|r| r.ok()).collect());
}
```

**Benefits**:
- Fingerprint matching becomes a testable, isolated service
- Can batch-match multiple windows in parallel
- Clear separation: matching (I/O) vs. applying (state mutation)
- Easy to swap matching strategies
- Lock hold time reduced

**Lines Affected**: 308-417 (110 lines → ~150 lines with clearer structure)

---

### 🟠 MEDIUM PRIORITY - Code Organization

#### 4. Split `daemon.rs` into Module: `daemon/mod.rs`

**Issue**: 1343 lines in a single file makes navigation difficult. The file contains:
- State structures (DaemonState, SummaryEntry)
- Event definitions (DaemonEvent)
- Main daemon loop
- IPC request handlers
- Workspace summarization
- Subscriber management

**Proposed Structure**:
```
src/daemon/
├── mod.rs              # Public API, module exports
├── state.rs            # DaemonState, SummaryEntry
├── events.rs           # DaemonEvent enum (internal events, not BabelEvent)
├── lifecycle.rs        # WindowLifecycle, WindowTransition
├── fingerprint.rs      # FingerprintMatcher service
├── handlers/
│   ├── mod.rs          # Handler registration
│   ├── windows.rs      # List, Status, Enrich, Focus
│   ├── wsets.rs        # WSet operations
│   ├── session.rs      # Tag, MarkRead, Scroll, Send
│   └── admin.rs        # Ping, Shutdown, Refresh, Titles
├── run.rs              # Main daemon loop (run_daemon, run_daemon_traced)
└── subscriber.rs       # handle_subscriber, subscription management
```

**Benefits**:
- Each file under 300 lines
- Clear module boundaries
- Easy to locate specific functionality
- Better compile-time parallelism
- Easier code reviews

**Lines Affected**: All 1343 lines → 7-8 focused modules

---

#### 5. Introduce `RequestHandler` Trait for Polymorphic Handlers

**Location**: Lines 928-1342
**Issue**: Adding a new IPC request requires editing a giant match statement. No compile-time guarantee that all requests are handled.

**Proposed Refactoring**:
```rust
#[async_trait]
trait RequestHandler: Send + Sync {
    async fn handle(
        &self,
        state: &Arc<RwLock<DaemonState>>,
        ctx: &HandlerContext,
    ) -> Response;
}

struct HandlerContext {
    summarizer: Arc<WorkspaceSummarizer>,
}

// Each request type gets its own handler
struct ListHandler;
#[async_trait]
impl RequestHandler for ListHandler {
    async fn handle(&self, state: &Arc<RwLock<DaemonState>>, _: &HandlerContext) -> Response {
        let s = state.read().await;
        let windows: Vec<ClaudeWindow> = s.windows.values().cloned().collect();
        Response::Windows { windows }
    }
}

struct WSetSaveHandler {
    name: Option<String>,
}
#[async_trait]
impl RequestHandler for WSetSaveHandler {
    async fn handle(&self, state: &Arc<RwLock<DaemonState>>, _: &HandlerContext) -> Response {
        // ... extracted logic
    }
}

// Registry maps Request → Handler
struct HandlerRegistry {
    handlers: HashMap<RequestType, Box<dyn RequestHandler>>,
}

impl HandlerRegistry {
    fn dispatch(&self, request: Request, state: &Arc<RwLock<DaemonState>>, ctx: &HandlerContext) -> Response {
        let handler = self.handlers.get(&request.request_type())
            .expect("Unhandled request type");
        handler.handle(state, ctx).await
    }
}
```

**Benefits**:
- Each handler is a separate struct (better testing)
- Easy to add middleware (logging, metrics, auth)
- Handlers can have per-request state
- Clear handler lifecycle
- Better error handling per handler type

**Caveat**: Adds trait complexity. Only worth it if handlers grow more complex.

**Lines Affected**: 928-1342 (414 lines → 20 handler structs + registry)

---

#### 6. Extract `rebuild_summary_index` and `rebuild_fingerprint_index` into `IndexBuilder` Service

**Location**: Lines 420-531 (111 lines)
**Issue**: Index rebuilding logic is embedded in DaemonState. Testing requires mocking filesystem. Lots of duplicated patterns.

**Proposed Refactoring**:
```rust
/// Service for building indices from ~/.claude
struct IndexBuilder {
    projects_dir: PathBuf,
}

impl IndexBuilder {
    fn new() -> Self {
        Self {
            projects_dir: claude_base().join("projects"),
        }
    }

    /// Build summary index (title → session mappings)
    fn build_summary_index(&self) -> Result<Vec<SummaryEntry>> {
        if !self.projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut index = Vec::new();

        for session_path in self.iter_session_files()? {
            // Skip agent sessions
            if self.is_agent_session(&session_path) {
                continue;
            }

            let session_id = self.extract_session_id(&session_path)?;

            if let Ok(info) = get_session_info(&session_path) {
                for summary in info.summaries {
                    index.push(SummaryEntry {
                        summary: summary.summary,
                        session_id: session_id.clone(),
                    });
                }
            }
        }

        Ok(index)
    }

    /// Build fingerprint index (top 100 recent sessions)
    fn build_fingerprint_index(&self) -> Result<HashMap<String, SessionFingerprint>> {
        let mut session_files = self.collect_session_files_with_mtime()?;

        // Sort by mtime, take top 100
        session_files.sort_by(|a, b| b.1.cmp(&a.1));
        session_files.truncate(100);

        let mut index = HashMap::new();
        for (path, _) in session_files {
            if let Some(session_id) = self.extract_session_id(&path).ok() {
                if let Ok(mut fp) = extract_from_jsonl(&path) {
                    fp.session_id = Some(session_id.clone());
                    index.insert(session_id, fp);
                }
            }
        }

        Ok(index)
    }

    // Helper methods
    fn iter_session_files(&self) -> Result<impl Iterator<Item = PathBuf>> { /* ... */ }
    fn is_agent_session(&self, path: &Path) -> bool { /* ... */ }
    fn extract_session_id(&self, path: &Path) -> Result<String> { /* ... */ }
    fn collect_session_files_with_mtime(&self) -> Result<Vec<(PathBuf, SystemTime)>> { /* ... */ }
}

// DaemonState becomes simpler
impl DaemonState {
    pub fn rebuild_summary_index(&mut self) -> Result<()> {
        let builder = IndexBuilder::new();
        self.summary_index = builder.build_summary_index()?;
        Ok(())
    }

    pub fn rebuild_fingerprint_index(&mut self) -> Result<()> {
        const REBUILD_DEBOUNCE: Duration = Duration::from_secs(2);
        if self.last_fingerprint_rebuild.elapsed() < REBUILD_DEBOUNCE {
            return Ok(());
        }

        let builder = IndexBuilder::new();
        self.fingerprint_index = builder.build_fingerprint_index()?;
        self.last_fingerprint_rebuild = Instant::now();

        tracing::info!(session_count = self.fingerprint_index.len(), "Rebuilt fingerprint index");
        Ok(())
    }
}
```

**Benefits**:
- IndexBuilder can be unit tested with temp directories
- Clear filesystem access patterns
- Easier to mock for testing
- Could be extracted to separate module
- Shared helper methods reduce duplication

**Lines Affected**: 420-531 (111 lines → ~150 lines with better structure)

---

### 🟡 LOW PRIORITY - Polish & Clarity

#### 7. Extract Event Publishing Patterns into Helper Methods

**Location**: Multiple locations (210-249 in refresh_windows, 831 in summarize_workspace)
**Issue**: Event publishing code is verbose and repetitive. Pattern: construct event → publish → log.

**Proposed Refactoring**:
```rust
impl DaemonState {
    /// Emit WindowAdded event and log
    fn emit_window_added(&self, kitty_id: u64, title: String, workspace: Option<i32>) {
        self.event_publisher.publish(BabelEvent::WindowAdded {
            kitty_id,
            title,
            workspace,
        });
        tracing::debug!(kitty_id, ?workspace, "Window added");
    }

    /// Emit SessionStateChanged event
    fn emit_state_changed(&self, kitty_id: u64, session_id: Option<String>, workspace: Option<i32>, old: SessionState, new: SessionState) {
        trace!("Window {} state change: {:?} -> {:?}", kitty_id, old, new);
        self.event_publisher.publish(BabelEvent::SessionStateChanged {
            kitty_id,
            session_id,
            workspace,
            old_state: old,
            new_state: new,
        });
    }

    // ... similar helpers for other events
}

// Usage becomes cleaner
// Before:
self.event_publisher.publish(BabelEvent::WindowAdded {
    kitty_id: id,
    title: w.title.clone(),
    workspace: w.workspace,
});

// After:
self.emit_window_added(id, w.title.clone(), w.workspace);
```

**Benefits**:
- Consistent event emission patterns
- Automatic logging
- Easier to add metrics/telemetry later
- Less boilerplate

**Lines Affected**: ~50 lines of event publishing code → ~100 lines (helpers + usage)

---

#### 8. Replace Magic Numbers with Named Constants

**Location**: Lines 627, 638, 645, 482
**Issue**: Timing and size constants scattered throughout without explanation.

**Current**:
```rust
tokio::time::interval(Duration::from_millis(500))  // Line 627
new_debouncer(Duration::from_millis(500), tx)      // Line 645
const REBUILD_DEBOUNCE: Duration = Duration::from_secs(2);  // Line 482
const MAX_FINGERPRINT_CACHE: usize = 100;          // Line 542
```

**Proposed**:
```rust
// Top of file
mod config {
    use std::time::Duration;

    /// Interval between kitty window polls (500ms = 2 Hz)
    /// Chosen to balance responsiveness vs. CPU usage
    pub const KITTY_POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// Debounce interval for file watcher events
    /// Prevents rapid-fire rebuilds when multiple files change
    pub const FILE_WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

    /// Minimum time between fingerprint index rebuilds
    /// Expensive operation, debounce to avoid thrashing
    pub const FINGERPRINT_REBUILD_DEBOUNCE: Duration = Duration::from_secs(2);

    /// Maximum number of sessions in fingerprint index
    /// Limited to 100 most recent to keep memory bounded
    pub const FINGERPRINT_INDEX_LIMIT: usize = 100;

    /// Maximum cached window fingerprints
    /// Safety net to prevent unbounded growth
    pub const FINGERPRINT_CACHE_LIMIT: usize = 100;
}

// Usage
tokio::time::interval(config::KITTY_POLL_INTERVAL)
new_debouncer(config::FILE_WATCH_DEBOUNCE, tx)
```

**Benefits**:
- Self-documenting code
- Easy to tune performance
- Clear rationale for each constant
- Central location for configuration

**Lines Affected**: ~10 lines

---

#### 9. Add Type Aliases for Complex Types

**Location**: Throughout
**Issue**: Repeated complex type signatures reduce readability.

**Proposed**:
```rust
// Before:
pub windows: HashMap<u64, ClaudeWindow>,
pub fingerprint_index: HashMap<String, SessionFingerprint>,
pub window_fingerprints: HashMap<u64, SessionFingerprint>,

// After:
type WindowId = u64;
type SessionId = String;
type WindowMap = HashMap<WindowId, ClaudeWindow>;
type SessionFingerprintIndex = HashMap<SessionId, SessionFingerprint>;
type WindowFingerprintCache = HashMap<WindowId, SessionFingerprint>;

pub windows: WindowMap,
pub fingerprint_index: SessionFingerprintIndex,
pub window_fingerprints: WindowFingerprintCache,
```

**Benefits**:
- Clearer intent
- Easier to change underlying types
- Better documentation

**Lines Affected**: ~20 lines of type definitions

---

#### 10. Extract Workspace Summarization into Separate Module

**Location**: Lines 774-838 (65 lines)
**Issue**: `summarize_workspace` is conceptually separate from daemon lifecycle. Mixing HTTP calls (Haiku API) with state management.

**Proposed**:
Move to `src/daemon/workspace_summary.rs`:
```rust
/// Summarize Claude sessions on a workspace
///
/// Generates human-readable titles like "refactoring auth system" via Haiku.
pub async fn summarize_workspace(
    workspace: i32,
    state: &Arc<RwLock<DaemonState>>,
    summarizer: &Arc<WorkspaceSummarizer>,
) -> Option<String> {
    // ... extracted logic
    // Returns title or None on error
}
```

**Benefits**:
- Clearer file organization
- Easier to test summarization in isolation
- Reduces daemon.rs line count

**Lines Affected**: 65 lines moved to separate module

---

## Code Duplication Analysis

### Pattern: Lock → Clone Windows → I/O → Return

Appears in:
- `handle_client` (Request::ListWithFingerprints) - Lines 940-962
- `process_request` (Request::Refresh) - Lines 1083-1116

**Proposed Helper**:
```rust
async fn enrich_windows_with_fingerprints(
    state: &Arc<RwLock<DaemonState>>
) -> Vec<ClaudeWindow> {
    let mut windows = {
        let s = state.read().await;
        s.windows.values().cloned().collect()
    };

    for win in &mut windows {
        if win.fingerprint.is_none() {
            if let Ok(scrollback) = get_scrollback(win.kitty_id) {
                win.fingerprint = Some(extract_from_scrollback(&scrollback));
            }
        }
        if win.session_info.is_none() {
            let _ = enrich_window(win);
        }
    }

    windows
}
```

---

### Pattern: Load WSet Name from File or Arg

Appears in:
- Request::WSetSave (Lines 1176-1179)
- Request::WSetLoad (Lines 1206-1218)

**Proposed Helper**:
```rust
fn resolve_wset_name(provided: Option<String>) -> Result<String> {
    match provided {
        Some(name) => Ok(name),
        None => get_current_wset_name()?
            .ok_or_else(|| anyhow::anyhow!("No current WSet. Specify a name."))
    }
}
```

---

## Testing Recommendations

After refactoring, the following should become testable:

1. **WindowLifecycle** - Unit test state transitions
2. **FingerprintMatcher** - Unit test matching logic with mock scrollback
3. **IndexBuilder** - Unit test with temp directories
4. **Request Handlers** - Integration test each handler with mock state

**Example Test**:
```rust
#[tokio::test]
async fn test_window_lifecycle_title_change() {
    let old = ClaudeWindow {
        kitty_id: 1,
        title: "Old Title".to_string(),
        session_id: Some("session-123".to_string()),
        // ...
    };

    let new = KittyWindow {
        id: 1,
        title: "New Title".to_string(),
        // ...
    };

    let lifecycle = WindowLifecycle {
        kitty_id: 1,
        old_state: Some(old),
        new_kitty_data: new,
    };

    let transition = lifecycle.plan_transition(&vec![]);

    assert!(matches!(transition, WindowTransition::TitleChanged { needs_rematch: true, .. }));
}
```

---

## Migration Path

### Phase 1: Extract Handlers (High Priority #1)
1. Create `src/daemon/handlers/` directory
2. Move each match arm to separate handler function
3. Update `process_request` to delegate
4. No behavioral changes, pure refactor

**Estimated Effort**: 4-6 hours
**Risk**: Low (no logic changes)

### Phase 2: Extract WindowLifecycle (High Priority #2)
1. Create `src/daemon/lifecycle.rs`
2. Define WindowTransition enum
3. Refactor `refresh_windows` to use lifecycle
4. Add unit tests

**Estimated Effort**: 6-8 hours
**Risk**: Medium (complex refactor, needs careful testing)

### Phase 3: Split Module (Medium Priority #4)
1. Create module structure
2. Move code to appropriate files
3. Update imports

**Estimated Effort**: 2-3 hours
**Risk**: Low (mechanical refactor)

### Phase 4: Extract Services (Medium Priority #3, #6)
1. Create FingerprintMatcher
2. Create IndexBuilder
3. Update callers

**Estimated Effort**: 4-5 hours
**Risk**: Medium (lock patterns change)

---

## Anti-Patterns Observed

### 1. God Object: DaemonState
**Lines**: 65-122, 127-551

DaemonState has 9 fields and 10+ methods doing different things:
- Window tracking
- Index management
- Fingerprint caching
- Event publishing
- State tracking

**Fix**: Extract into smaller, focused structs (WindowRegistry, FingerprintMatcher, EventPublisher)

### 2. Anemic Domain Model: ClaudeWindow
**File**: `src/discovery.rs` Lines 31-52

ClaudeWindow is a pure data struct with no behavior. Operations on windows are scattered across modules.

**Fix**: Add methods to ClaudeWindow:
```rust
impl ClaudeWindow {
    fn needs_fingerprint(&self) -> bool {
        self.session_id.is_none()
    }

    fn is_agent_session(&self) -> bool {
        self.session_id.as_ref().map_or(false, |id| id.starts_with("agent-"))
    }

    fn should_rematch(&self) -> bool {
        self.is_agent_session() || self.session_id.is_none()
    }
}
```

### 3. Primitive Obsession: Window IDs, Session IDs

Using raw `u64` and `String` everywhere makes it easy to mix up parameters.

**Fix**: Use newtype wrappers:
```rust
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
struct WindowId(u64);

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct SessionId(String);
```

---

## Performance Considerations

### Lock Contention Analysis

Current worst-case lock hold times:

1. **refresh_windows**: ~50ms (kitty ls + workspace queries + state updates)
2. **fingerprint_match**: 100-500ms (get_scrollback is slow)
3. **rebuild_fingerprint_index**: 1-5s (reads 100 JSONL files)

**Recommendations**:
- Keep using the "read → I/O → write" pattern
- Consider RwLock → parking_lot::RwLock for better performance
- Add lock hold time metrics

---

## Summary

| Refactoring | Priority | Effort | Risk | Impact |
|-------------|----------|--------|------|---------|
| Extract handlers | 🔴 High | 4-6h | Low | Testability, clarity |
| WindowLifecycle | 🔴 High | 6-8h | Medium | Maintainability |
| FingerprintMatcher | 🔴 High | 4-5h | Medium | Concurrency, testing |
| Split module | 🟠 Medium | 2-3h | Low | Navigation |
| RequestHandler trait | 🟠 Medium | 6-8h | High | Extensibility |
| IndexBuilder | 🟠 Medium | 4-5h | Low | Testing |
| Event helpers | 🟡 Low | 1-2h | Low | Consistency |
| Named constants | 🟡 Low | 1h | Low | Clarity |
| Type aliases | 🟡 Low | 1h | Low | Documentation |
| Extract summarization | 🟡 Low | 1h | Low | Organization |

**Total Estimated Effort**: 30-45 hours for all refactorings

**Recommended Order**:
1. Named constants + type aliases (quick wins)
2. Extract handlers (big clarity gain)
3. Split module (easier navigation for next steps)
4. WindowLifecycle (complex but high value)
5. Services (FingerprintMatcher, IndexBuilder)
6. Advanced patterns (RequestHandler trait) - only if needed

---

**End of Report**
