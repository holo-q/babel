# Babel

Unified interface for managing agent sessions across kitty terminal panes.

## Overview

Babel provides discovery, tracking, and control of agent sessions running in terminal panes. Today, that means kitty. It uses hooks as the live protocol for lifecycle state, the terminal adapter as the source of truth for pane presence/focus, and fingerprinting as cold-start recovery when Babel starts after sessions are already in flight.

## Features

- **Session Discovery**: Find active agent sessions across kitty panes
- **Storage Integration**: Access provider conversation history and Babel metadata
- **Pane Control**: Send commands, focus panes, scroll output via kitty protocol
- **Overlay Metadata**: Track custom icons, read status, notes (separate from Claude storage)
- **State Detection**: Identify session state (idle, thinking, tool use, awaiting input)
- **Fire Mode**: Quick-launch agents with smart working directory detection

## Terminal Support

Kitty is the only supported terminal today. This is a capability boundary, not a preference: Babel needs stable pane identity, pane-local environment/context, remote control for focus/text/scrollback/title/user metadata, and enough socket or instance information to route commands without guessing.

Terminal support should follow the same adapter discipline as harness support. All terminals are considered on equal terms, but an adapter must expose the necessary context honestly before Babel can orchestrate it. In theory, tmux is the nearest candidate because it has explicit pane identity and control primitives. Zellij may also be viable if its action/plugin surface can provide the same stable context. Other terminals are unsupported until they can provide equivalent signals instead of cwd/time/title guessing.

| Terminal | Status | Notes |
| --- | --- | --- |
| kitty | Supported | Current reference terminal adapter. Provides remote control, pane ids, sockets, scrollback, titles, user vars/tags, and multi-instance routing. |
| tmux | Candidate | Likely first non-kitty target if pane identity, scrollback, environment, and command routing can be mapped cleanly. |
| zellij | Candidate | Potentially viable after proving stable pane identity and control/context APIs. |
| other terminals | Unsupported | Considered equally, but blocked until they expose the required orchestration context. |

## Panel Plugins

Babel exposes a typed **paint stream** — subscribers receive
`PaintEvent::Window` / `PaintEvent::Workspace` payloads with all per-pane
UX truth (color, ring intensity, scale, outline, x position) already
resolved by the daemon. Consumers don't classify state or pick colors;
they just relay bytes to a panel-side renderer.

| Project | Surface | What it does |
| --- | --- | --- |
| [xfce4-panel-richmon-babel](https://github.com/holo-q/xfce4-panel-richmon-babel) | XFCE panel daemon | Forwards `PaintEvent::Window` to the [richmon](https://github.com/holo-q/xfce4-panel-richmon) panel widget — a dot per pane, colored by harness/state |
| [xfce4-panel-richspace-babel](https://github.com/holo-q/xfce4-panel-richspace-babel) | XFCE panel daemon | Aggregates per-workspace ring intensity and animates transitions for the [richspace](https://github.com/holo-q/xfce4-panel-richspace) workspace switcher |

The protocol is in `babel::paint::PaintEvent` — PRs welcome for
tmux/Wayland/i3bar/waybar/polybar equivalents; the typed surface is
stable and clients only need a Unix socket and JSON.

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
babel mv --doctor ~/old ~/new # Universal evidence report; no mutation
babel mv --dry ~/old ~/new    # Legacy Claude-only preview
babel mv --history-only       # Legacy Claude-only history update

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
- **harness_ops**: Harness-aware operation planning for move/resume-style workflows.
  `--doctor` is the first public surface: it audits native storage and live panes
  without using a global index or the legacy Claude-only mover.

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

### Deprecated And Consumed

These projects are prior art and reference material. Babel's direction is to consume the useful jutsus into one harness-aware system: move, resume, handoff, search, archive, migration, bridge contracts, and verification. Once absorbed, they are deprecated as standalone answers for this stack, not dismissed as bad work.

Local clones live under ignored `references/` when doing adapter work. The clones are a research board, not vendored code; durable conclusions belong in `docs/18-harness-operations-board.md`, the harness roster, tests, or adapter comments.

| Project | Link | Jutsus to consume |
| --- | --- | --- |
| `claudepath` | https://pypi.org/project/claudepath/ | Claude project move/remap, dry-run, merge, backups, restore |
| `ccmv` | https://github.com/seflue/ccmv | Claude mv semantics, settings/MCP/trust rewrite, nested subproject handling, rollback |
| `cc-port` | https://github.com/it-bens/cc-port | Claude move/export/import/push/pull, manifests, archives, rollback, live-session checks |
| Project Directory Migration skill | https://mcpmarket.com/tools/skills/project-directory-migration | Migration checklist, backup discipline, symlink compatibility, environment fixups |
| `cursor-history` | https://github.com/S2thend/cursor-history | Cursor session browse/search/export/backup/restore/migration |
| `cursor-chat-recovery-kit` | https://github.com/vitalyis/cursor-chat-recovery-kit | Cursor workspace rename recovery and chat reattachment |
| SpecStory Cursor integration | https://docs.specstory.com/integrations/cursor | Repo-local conversation archive via `.specstory/history/` |
| WayLog | https://marketplace.visualstudio.com/items?itemName=waylog.waylog | Repo-local AI chat archive across Cursor, Copilot, Roo, Cline, Kilo, Codex extension, Lingma, CodeBuddy |
| `cdxresume` | https://github.com/sasazame/cdxresume | Codex session browser/resume over `~/.codex/sessions` |
| `cli-continues` | https://github.com/yigitkonur/cli-continues | Cross-agent handoff/resume docs across the full harness roster |
| CASR | https://github.com/Dicklesworthstone/cross_agent_session_resumer | Canonical IR, provider-native session conversion, native resume targets |
| Codbash | https://github.com/vakovalskii/codbash | Multi-harness dashboard, search, replay, resume, tag, import/export, handoff, Claude/Codex conversion |
| `mnemo` | https://github.com/Pilan-AI/mnemo | Native storage parsers, SQLite/FTS index, MCP context tools |
| `coding_agent_session_search` | https://github.com/Dicklesworthstone/coding_agent_session_search | Broad multi-harness session search and TUI patterns |
| CCManager | https://github.com/kbwo/ccmanager | Session/worktree management and Claude context copy into new worktrees |
| Chronicle / `claude-history-manager` | https://github.com/josephyaduvanshi/claude-history-manager | Native desktop browsing/indexing/resume for Claude, Codex, Gemini |
| HAPI | https://github.com/tiann/hapi | Remote/mobile controller patterns for Claude, Codex, Cursor, Gemini, OpenCode |
| Agor | https://github.com/preset-io/agor | Multiplayer session canvas, worktrees, conversation tracking |
| Agent Deck | https://github.com/asheshgoplani/agent-deck | Terminal session manager patterns across Claude, Gemini, OpenCode, Codex |
| AgentOS | https://github.com/saadnvd1/agent-os | Mobile-first web UI for CLI agents |
| PAL MCP | https://github.com/BeehiveInnovations/pal-mcp-server | Multi-CLI bridge, context continuity, CLI-to-CLI handoff |
| Iranti | https://www.iranti.dev/ | Shared durable memory layer across agents and MCP/HTTP clients |
| SessionBase | https://github.com/sessionbase/sessionbase | Cross-agent session listing/push/export substrate |

## Integration Points

### Terminal Protocol

The only implemented terminal adapter is kitty, using `kitten @` commands for remote control:
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
- [ ] Conversation pager TUI (spec in docs/17-conversation-pager-spec.md)

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
