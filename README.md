# Babel

Unified interface for managing agent sessions across kitty terminal panes.

## Overview

Babel provides discovery, tracking, and control of agent sessions running in kitty panes. It uses hooks as the live protocol for lifecycle state, kitty as the source of truth for pane presence/focus, and fingerprinting as cold-start recovery when Babel starts after sessions are already in flight.

## Features

- **Session Discovery**: Find active agent sessions across kitty panes
- **Storage Integration**: Access provider conversation history and Babel metadata
- **Pane Control**: Send commands, focus panes, scroll output via kitty protocol
- **Overlay Metadata**: Track custom icons, read status, notes (separate from Claude storage)
- **State Detection**: Identify session state (idle, thinking, tool use, awaiting input)
- **Fire Mode**: Quick-launch agents with smart working directory detection

## Building

```bash
cargo build --release
```

Binary output: `target/release/babel`

## CLI Usage

```bash
# List all discovered agent sessions (scans all kitty instances by default)
babel ls              # Compact view with activity state indicators
babel ls -d           # Detailed view with fingerprint data
babel ls --all        # Include ordinary/unrecognized terminal panes

# Check specific pane status
babel get-window 42   # By pane ID
babel get-window      # Focused agent pane

# Pane control
babel focus 42        # Focus pane (or omit ID for rofi picker)
babel send 42 "text"  # Send text to pane
babel send * "text"   # Broadcast to all agent panes

# Session metadata
babel set-icon 42 🔥   # Custom icon indicator
babel set-read 42      # Mark as read
babel set-title 42     # Auto-title from session
babel set-title * "My Title"  # Custom title

# View conversation history
babel history         # Recent conversations
babel history -l 50   # Limit results
babel history abc123  # Specific session ID

# Fire-and-forget agent launch
babel fire "Implement feature X"           # Auto-detect CWD
babel fire -d ~/project "Fix the bug"      # Explicit directory
babel fire-ls                              # List running fire tasks
babel fire-clean                           # Clean up finished tasks

# Directory migration (preserves provider history)
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
- **agent_discovery**: Correlate kitty panes with agent sessions
- **overlay**: User metadata database (icons, read status, notes)
- **state**: Detect session activity state from scrollback
- **fire**: Quick agent launch with smart directory detection

### Data Storage

- **Claude data**: `~/.claude/` (managed by Claude Code, read-only)
  - `history.jsonl` - Global session index
  - `projects/{project}/{sessionId}.jsonl` - Full conversations

- **Babel overlay**: `~/.local/share/babel/` (managed by babel)
  - `overlay.db` - User metadata (icons, read status, notes)
  - `state.json` - Cached session state (performance)

## Harness Support

Babel's integration bar is intentionally simple: a harness must expose lifecycle hooks and a stable session identity. Without that, Babel can still show the kitty pane, but it cannot honestly orchestrate unread state, finished/working transitions, or persistent session identity.

Live binding uses:

```text
harness:native-session-id + KITTY_LISTEN_ON + KITTY_WINDOW_ID
```

Babel stores hook identities as namespaced keys such as `claude:<uuid>`, `cursor:<conversation_id>`, and `cline:<taskId>`. Provider-native ids remain available at the adapter boundary for storage lookup, but the live pane index, overlay metadata, read state, and paint stream use the namespaced key so harnesses cannot collide.

Cold-start recovery, for sessions already running before Babel starts, uses durable kitty tags first and scrollback fingerprinting only as a fallback.

Hook behavior is registry-driven. Each harness declares native event names, the canonical Babel event, optional state transition, read/unread effect, and pulse effect. Partial hook surfaces are valid: a harness can emit a tool pulse without pretending it has Stop/Prompt hooks.

`babel hook install` writes known-safe local targets and prints exact snippets for harness-owned config surfaces. Bridge-only harnesses get the canonical callback contract instead of guessed provider plugins.

Bridge callback payload:

```json
{
  "session_id": "stable-native-id",
  "tool_name": "optional-tool",
  "cwd": "optional-working-directory"
}
```

Invoke bridge callbacks through:

```bash
babel hook stdin <canonical-event> --agent <harness-slug>
```

Accent swatches are generated from `tools/generate-swatches.py` into `docs/swatches/`.

Maintenance level is separate from adapter support. Claude Code and Codex CLI are daily-driver paths and should work out of the box. The rest are rostered adapters or contracts that need real users; if they break, expect to open a ticket with traces, study the harness behavior, or propose a PR.

| Harness | Accent | Identity field | Lifecycle hooks | Babel support | Maintenance level |
| --- | --- | --- | --- | --- | --- |
| Claude Code | ![Claude Code accent](docs/swatches/claude-code.svg) `#D97757` | `session_id` | Full | First-class reference dialect | Daily driver; guaranteed main feature |
| Codex CLI | ![Codex CLI accent](docs/swatches/codex-cli.svg) `#10A37F` | `session_id` | Partial/full by version | Supported through canonical hook adapter | Daily driver; guaranteed main feature |
| Factory Droid | ![Factory Droid accent](docs/swatches/factory-droid.svg) `#D15010` | `session_id` | Claude-compatible | Supported | Untested roster adapter; user reports/PRs needed |
| Qwen Code | ![Qwen Code accent](docs/swatches/qwen-code.svg) `#624BEA` | `session_id` | Claude-compatible | Supported | Untested roster adapter; user reports/PRs needed |
| Kimi CLI | ![Kimi CLI accent](docs/swatches/kimi-cli.svg) `#7F1C10` | `session_id` | Same vocabulary, TOML config | Supported with config translation | Untested roster adapter; user reports/PRs needed |
| Gemini CLI | ![Gemini CLI accent](docs/swatches/gemini-cli.svg) `#4285F4` | `session_id`, `GEMINI_SESSION_ID` | Name-mapped | Supported with event-name adapter | Untested roster adapter; user reports/PRs needed |
| Crush | ![Crush accent](docs/swatches/crush.svg) `#6B50FF` | `session_id`, `CRUSH_SESSION_ID` | Partial today | Supported for emitted lifecycle events | Untested roster adapter; partial surface expected |
| Cursor Agent | ![Cursor Agent accent](docs/swatches/cursor-agent.svg) `#14120B` / `#F7F7F4` | `conversation_id` | Name-mapped | Supported with identity-field adapter | Untested roster adapter; user reports/PRs needed |
| Cline | ![Cline accent](docs/swatches/cline.svg) `#9663F0` | `taskId` | Different task lifecycle | Supported as task identity, not Claude-style session identity | Untested roster adapter; task semantics need users |
| OpenCode | ![OpenCode accent](docs/swatches/opencode.svg) `#FAB283` | Plugin callback API | In-process callbacks | Bridge required; same canonical events after bridge | Contract only; needs harness-side bridge work |
| Amp | ![Amp accent](docs/swatches/amp.svg) `#F34E3F` | Plugin callback API | In-process callbacks | Bridge required; same canonical events after bridge | Contract only; needs harness-side bridge work |
| Kiro | ![Kiro accent](docs/swatches/kiro.svg) `#C6A0FF` | Form/API specific | Hybrid IDE lifecycle | Bridge required; same canonical events after bridge | Contract only; needs harness-side bridge work |

### Dunce List

These harnesses do not meet the orchestration bar. Babel should not contort itself around cwd/time guesses for them.

| Harness | Accent | Status | Reason | Maintenance level |
| --- | --- | --- | --- | --- |
| GitHub Copilot CLI | ![GitHub Copilot CLI accent](docs/swatches/github-copilot-cli.svg) `#8250DF` | Unsupported | Documented stdin hook payload has no stable session/task identifier | None until the harness grows a stable identity |
| Roo Code | ![Roo Code accent](docs/swatches/roo-code.svg) `#D8F14B` | Unsupported | No lifecycle hooks | None until hooks exist |
| Kilo Code | ![Kilo Code accent](docs/swatches/kilo-code.svg) `#FA483A` | Unsupported | No lifecycle hooks | None until hooks exist |
| Aider | ![Aider accent](docs/swatches/aider.svg) `#14B014` | Unsupported | No lifecycle hooks | None until hooks exist |
| Antigravity | ![Antigravity accent](docs/swatches/antigravity.svg) `#3186FF` | Unsupported | Rules/workflows exist, but no lifecycle hook surface | None until lifecycle hooks exist |

## Integration Points

### Kitty Protocol

Uses `kitten @` commands for remote control:
- `ls` - Query kitty pane/window state
- `send-text` - Send input to panes
- `focus-window` - Activate panes
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
- [x] Session discovery via hooks, tags, fingerprinting, and scrollback recovery
- [x] Activity state detection (idle, thinking, tool use, awaiting input)
- [x] Overlay metadata (icons, read/unread, chapter history, notes)
- [x] wmctrl-like pane control (focus, send-text, set-title)
- [x] Fire mode with smart CWD detection
- [x] `babel mv` - directory migration preserving Claude history
- [x] WSet save/load for workspace layouts
- [x] Daemon mode with IPC and event streaming
- [x] Multi-socket discovery (default, with fenced operations)
- [x] Auto-unread on AwaitingInput state change

### In Progress
- [ ] Simplified prompt representations via Haiku summarization (40%)
- [ ] Conversation pager TUI (spec in Docs/17-conversation-pager-spec.md)

### Planned
- [ ] Launch agents with workspace context (ambient awareness)
- [ ] Captain orchestration (multi-session coordination)
- [ ] Performance profiling for large conversation histories

## Design Notes

### Separation of Concerns

Babel maintains user-specific metadata (icons, notes, read status) in a separate database (`overlay.db`) rather than modifying provider conversation files. This:
- Preserves Claude storage integrity
- Enables independent backups
- Avoids conflicts with Claude updates
- Allows clean uninstall

### Performance

- **Lazy parsing**: Conversation files are streamed, not fully loaded
- **Caching**: Session state cached in `state.json` (with staleness checks)
- **Summaries first**: Read only first ~20 lines for quick metadata extraction
- **Pane filtering**: Only queries provider storage for confirmed agent panes

### State Detection Heuristics

Session state is inferred from scrollback patterns:
- **Idle**: Prompt ending with `>`, `➜`, `$`
- **Thinking**: Spinner chars (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) or "thinking..."
- **ToolUse**: "Running", "Executing", tool blocks
- **AwaitingInput**: Questions ending with `?`, approval prompts

Heuristics are conservative - defaults to `Unknown` when ambiguous.

## License

Personal use - part of nuck's dotfiles workspace
