# Claude Babel Architecture

> Unified interface for Claude Code sessions across kitty terminal panes

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│  CLI Layer (thin puppets)                                       │
│  babel ls, babel focus, babel send, etc.                        │
└─────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  BabelCore (the brain)                                          │
│  - Unified API: windows(), focus(), history(), etc.             │
│  - Transparently handles daemon OR ephemeral mode               │
└─────────────────────────────────────────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
┌─────────────────────────┐   ┌─────────────────────────┐
│  Daemon Mode            │   │  Ephemeral Mode         │
│  - IPC to babeld        │   │  - Direct kitty/file    │
│  - Cached, instant      │   │  - On-demand loading    │
└─────────────────────────┘   └─────────────────────────┘
              │                           │
              └─────────────┬─────────────┘
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  kitty.rs (generic terminal control)                            │
│  - Socket discovery, pane listing, remote control               │
│  - Multi-instance aware (panes carry their socket)              │
└─────────────────────────────────────────────────────────────────┘
                            │
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
┌───────────────────┐ ┌───────────┐ ┌───────────────────┐
│ claude_discovery  │ │ claude_   │ │ fingerprint       │
│ - Claude detection│ │ storage   │ │ - Scrollback hash │
│ - Session matching│ │ - ~/.claude│ │ - JSONL matching  │
└───────────────────┘ └───────────┘ └───────────────────┘
```

## Module Hierarchy

```
src/
├── lib.rs              # Public API surface
├── core.rs             # BabelCore - unified entry point
├── daemon.rs           # babeld background service
├── kitty.rs            # Generic kitty terminal control
├── events.rs           # Event pub/sub for frontends
├── fingerprint.rs      # Scrollback↔JSONL matching
├── summarizer.rs       # LLM-based session summarization
├── wset.rs             # Workspace set save/restore
├── babel_storage.rs    # Babel's own persistent state
├── fire.rs             # claude-fire launcher
│
├── utility/
│   ├── claude_storage.rs    # ~/.claude/projects/ access
│   ├── claude_discovery.rs  # Claude session detection
│   └── ipc.rs               # Unix socket IPC helpers
│
└── cli/
    ├── main.rs         # Dispatcher (thin)
    ├── query.rs        # Read-only commands
    ├── action.rs       # State-mutating commands
    ├── mv.rs           # Session move/merge
    └── ...
```

### Separation of Concerns

**`kitty.rs`** - Generic terminal operations
- Socket discovery and multi-instance support
- Pane listing, focusing, text sending
- Scrollback retrieval
- User variable management
- Workspace integration (wmctrl)
- **No Claude-specific logic**

**`utility/claude_discovery.rs`** - Claude-specific detection
- `ClaudeSignals` - detection heuristics (process, title, tags)
- `ClaudeWindow` - enriched pane with session info
- `find_claude_windows()` - filter panes running Claude
- `spawn_claude_session()` - launch new sessions
- `load_wset()` - restore workspace sets

**`utility/claude_storage.rs`** - Claude's data access
- Read `~/.claude/projects/` structure
- Parse conversation JSONL files
- Session info extraction
- Summary search for matching

## Data Model

### KittyPane - The Fundamental Unit

```rust
pub struct KittyPane {
    /// Socket for the kitty instance this pane belongs to
    /// Enables operations to target the correct instance
    pub socket: String,

    /// Unique ID within the kitty instance
    /// NOTE: IDs are only unique per-instance, not globally
    pub id: u64,

    pub title: String,
    pub cwd: PathBuf,
    pub is_focused: bool,
    pub is_active: bool,
    pub foreground_processes: Vec<ForegroundProcess>,
    pub user_vars: HashMap<String, String>,

    /// Kitty's internal OS window ID
    pub os_window_id: u64,
    /// X11/Wayland window ID for workspace operations
    pub platform_window_id: u64,
}
```

**Design Rationale:**
- Panes carry their socket so operations always target the right instance
- Methods on `KittyPane` are self-targeted: `pane.focus()`, `pane.send_text()`
- Avoids confusion between kitty "windows" (panes) and OS windows

### ClaudeWindow - Enriched Pane

```rust
pub struct ClaudeWindow {
    /// The underlying kitty pane
    pub kitty_id: u64,
    pub socket: String,
    pub title: String,
    pub cwd: PathBuf,

    /// Matched session from ~/.claude
    pub session_id: Option<String>,
    pub session_info: Option<SessionInfo>,

    /// Detection signals
    pub signals: ClaudeSignals,

    /// Fingerprint for session matching
    pub fingerprint: Option<SessionFingerprint>,
}
```

### ClaudeSignals - Detection Heuristics

```rust
pub struct ClaudeSignals {
    /// "claude" in foreground process cmdline (definitive)
    pub process_running: bool,
    /// "✳" prefix in title (Claude's active indicator)
    pub title_indicator: bool,
    /// Has babel_session_id user_var (previously tagged)
    pub babel_tagged: bool,
    /// Session ID if tagged
    pub session_id: Option<String>,
}
```

A pane is a Claude session if ANY signal is positive.

## API Design Patterns

### Multi-Instance Awareness

```rust
// Default socket (most common case)
list_panes() -> Vec<KittyPane>
get_pane(id) -> Option<KittyPane>

// All instances (for discovery/debugging)
list_all_panes() -> Vec<KittyPane>
get_pane_all(id) -> Option<KittyPane>

// Specific socket (for targeted operations)
list_panes_from_socket(socket) -> Vec<KittyPane>
```

### Self-Targeted Methods

Once you have a `KittyPane`, operations use its socket automatically:

```rust
let pane = get_pane(42)?;
pane.focus()?;           // Uses pane.socket internally
pane.send_text("hello")?;
pane.set_title("New Title")?;
```

### Legacy Convenience (Default Socket)

For quick one-off operations on the default instance:

```rust
focus_window(42)?;        // Uses default_socket()
send_text(42, "hello")?;
set_window_title(42, "Title")?;
```

## Naming Conventions

| Term | Meaning |
|------|---------|
| **Pane** | A kitty "window" - the splits within a terminal |
| **OS Window** | The actual window on screen (can contain many panes) |
| **Socket** | Path to kitty's control socket (one per instance) |
| **Instance** | A running kitty process |
| **Session** | A Claude Code conversation (in ~/.claude) |

### Why "Pane" not "Window"?

Kitty's terminology uses "window" for what other terminals call panes. This causes confusion with OS-level windows. We use:
- `KittyPane` for kitty's windows (the splits)
- `os_window_id` / `platform_window_id` for actual windows

## Socket Discovery

```rust
// Find the default socket
default_socket() -> String
// Priority:
// 1. First kitty.sock-* in XDG_RUNTIME_DIR
// 2. Fallback to kitty.sock (usually doesn't exist)

// Find all sockets (for multi-instance)
find_all_sockets() -> Vec<String>

// Extract PID from socket path
socket_pid("unix:/run/user/1000/kitty.sock-12345") -> Some(12345)
```

## Event System

The daemon publishes events for real-time UI updates:

```rust
pub enum BabelEvent {
    WindowCreated { id: u64, title: String },
    WindowClosed { id: u64 },
    WindowFocused { id: u64 },
    SessionMatched { window_id: u64, session_id: String },
    SessionActivity { session_id: String, state: ActivityState },
    // ...
}
```

Subscribers register with filters:

```rust
pub struct EventFilter {
    pub window_events: bool,
    pub session_events: bool,
    pub specific_windows: Option<Vec<u64>>,
}
```

## Fingerprinting

Matches scrollback content to JSONL conversation files:

1. Extract characteristic patterns from scrollback
2. Hash conversation turns from JSONL
3. Find overlapping fingerprints
4. Score by match confidence

This handles cases where:
- Window was closed and reopened
- Session was resumed in a different pane
- Title doesn't match summary

## Workspace Sets (WSet)

Save and restore window arrangements:

```rust
pub struct WSet {
    pub name: String,
    pub sessions: Vec<WSetSession>,
}

pub struct WSetSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub workspace: Option<i32>,
}
```

## Future Considerations

### BabelCore as State Owner

Currently `spawn_claude_session()` and `load_wset()` live in `claude_discovery`. These should migrate to `BabelCore` methods since they:
- Need to immediately await and handle fingerprinting
- Should dispatch events
- Must update internal state

### Instance Collision

If multiple kitty instances have panes with the same ID, `get_pane_all()` returns the first match (preferring current instance). Consider:
- Composite keys: `(socket, id)`
- Or enforce single-instance via socket file locking
