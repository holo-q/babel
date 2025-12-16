# claude-babel

Unified interface for managing Claude Code sessions across kitty terminal windows.

## Overview

Babel provides discovery, tracking, and control of Claude Code sessions running in different kitty windows. It integrates with both kitty's remote control protocol and Claude's conversation storage to correlate windows with their associated conversation data.

## Features

- **Session Discovery**: Find all active Claude Code sessions across kitty windows
- **Storage Integration**: Access Claude's conversation history and metadata
- **Window Control**: Send commands, focus windows, scroll output via kitty protocol
- **Overlay Metadata**: Track custom icons, read status, notes (separate from Claude storage)
- **State Detection**: Identify session state (idle, thinking, tool use, awaiting input)
- **Fire Mode**: Quick-launch Claude with smart working directory detection

## Building

```bash
cargo build --release
```

Binary output: `target/release/babel`

## CLI Usage

```bash
# List all discovered Claude sessions
babel ls              # Compact view with activity state indicators
babel ls -d           # Detailed view with fingerprint data
babel ls --all        # Scan all kitty instances

# Check specific window status
babel get-window 42   # By window ID
babel get-window      # Focused Claude window

# Window control
babel focus 42        # Focus window (or omit ID for rofi picker)
babel send 42 "text"  # Send text to window
babel send * "text"   # Broadcast to all windows

# Session metadata
babel set-icon 42 🔥   # Custom icon indicator
babel set-read 42      # Mark as read
babel set-title 42     # Auto-title from session
babel set-title * "My Title"  # Custom title

# View conversation history
babel history         # Recent conversations
babel history -l 50   # Limit results
babel history abc123  # Specific session ID

# Fire-and-forget Claude launch
babel fire "Implement feature X"           # Auto-detect CWD
babel fire -d ~/project "Fix the bug"      # Explicit directory
babel fire-ls                              # List running fire tasks
babel fire-clean                           # Clean up finished tasks

# Directory migration (preserves Claude history)
babel mv ~/old ~/new          # Move + update history paths
babel mv --dry ~/old ~/new    # Preview changes only
babel mv --history-only       # Just update history

# Workspace sets
babel wset save mysetup       # Save current layout
babel wset load mysetup       # Restore layout
babel wset ls                 # List saved sets

# Daemon mode
babel daemon           # Start daemon (use systemctl for production)
babel monitor          # Stream daemon events
babel tui              # Interactive debug console
```

## Architecture

### Modules

- **claude_storage**: Parse Claude's JSONL conversation files and history
- **kitty**: Interface to kitty's remote control protocol
- **discovery**: Correlate kitty windows with Claude sessions
- **overlay**: User metadata database (icons, read status, notes)
- **state**: Detect session activity state from scrollback
- **fire**: Quick Claude launch with smart directory detection

### Data Storage

- **Claude data**: `~/.claude/` (managed by Claude Code, read-only)
  - `history.jsonl` - Global session index
  - `projects/{project}/{sessionId}.jsonl` - Full conversations

- **Babel overlay**: `~/.local/share/babel/` (managed by babel)
  - `overlay.db` - User metadata (icons, read status, notes)
  - `state.json` - Cached session state (performance)

## Integration Points

### Kitty Protocol

Uses `kitten @` commands for remote control:
- `ls` - Query window state
- `send-text` - Send input to windows
- `focus-window` - Activate windows
- `get-text` - Extract scrollback for state detection

### Claude Storage

Parses Claude's JSONL conversation format:
- Streaming parser (avoids loading entire files)
- Extracts summaries, metadata, working directories
- Fuzzy search across conversation history

## Development Status

**Current**: Feature-complete CLI with daemon mode

### Completed
- [x] Kitty protocol wrappers (remote control, scrollback, user_vars)
- [x] Session discovery via fingerprinting + scrollback analysis
- [x] Activity state detection (idle, thinking, tool use, awaiting input)
- [x] Overlay metadata (icons, read/unread, chapter history, notes)
- [x] wmctrl-like window control (focus, send-text, set-title)
- [x] Fire mode with smart CWD detection
- [x] `babel mv` - directory migration preserving Claude history
- [x] WSet save/load for workspace layouts
- [x] Daemon mode with IPC and event streaming
- [x] Multi-socket scan (`babel ls --all`)
- [x] Auto-unread on AwaitingInput state change

### In Progress
- [ ] Simplified prompt representations via Haiku summarization (40%)
- [ ] Conversation pager TUI (spec in Docs/17-conversation-pager-spec.md)

### Planned
- [ ] Launch Claude with workspace context (ambient awareness)
- [ ] Captain Claude orchestration (multi-session coordination)
- [ ] Performance profiling for large conversation histories

## Design Notes

### Separation of Concerns

Babel maintains user-specific metadata (icons, notes, read status) in a separate database (`overlay.db`) rather than modifying Claude's conversation files. This:
- Preserves Claude storage integrity
- Enables independent backups
- Avoids conflicts with Claude updates
- Allows clean uninstall

### Performance

- **Lazy parsing**: Conversation files are streamed, not fully loaded
- **Caching**: Session state cached in `state.json` (with staleness checks)
- **Summaries first**: Read only first ~20 lines for quick metadata extraction
- **Window filtering**: Only queries Claude storage for confirmed Claude processes

### State Detection Heuristics

Session state is inferred from scrollback patterns:
- **Idle**: Prompt ending with `>`, `➜`, `$`
- **Thinking**: Spinner chars (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) or "thinking..."
- **ToolUse**: "Running", "Executing", tool blocks
- **AwaitingInput**: Questions ending with `?`, approval prompts

Heuristics are conservative - defaults to `Unknown` when ambiguous.

## License

Personal use - part of nuck's dotfiles workspace
