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
babel list

# Show status of a window
babel status <window_id>

# Focus a Claude window
babel focus <window_id>

# Send text to a window
babel send <window_id> "Your prompt here"

# Tag a window with custom icon
babel tag <window_id> 🔥

# Mark conversation as read
babel mark-read <window_id>

# View conversation history
babel history --limit 10

# Open a specific session
babel open <session_id>

# Fire-and-forget Claude launch (smart CWD detection)
babel fire

# Fire with explicit directory
babel fire /path/to/project
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

**Current**: Project scaffold with module stubs

**TODO**:
- [ ] Implement kitty protocol wrappers
- [ ] Complete session discovery correlation logic
- [ ] Build overlay UI for session selection
- [ ] Integrate state detection into CLI commands
- [ ] Implement full fire.rs CWD detection from kitty JSON
- [ ] Add tests for discovery heuristics
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
