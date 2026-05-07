# Babel

Harness-aware substrate for managing AI-agent sessions across terminal panes.

## Overview

Babel is the local control plane for CLI/IDE agent sessions: live pane discovery,
native session history, resume ergonomics, lifecycle hooks, migration, paint
streams, and panel integrations. Hooks are the live protocol for lifecycle
truth, terminal adapters are the source of truth for pane presence and focus,
and fingerprinting is cold-start recovery for sessions that were already
running before Babel started.

The reference path is Claude Code and Codex CLI running in kitty. The broader
shape is deliberately harness-aware: every provider keeps its own native
storage and identity, while Babel owns orchestration, overlay metadata, durable
read state, terminal routing, and cross-harness UX.

## Features

### Session Control

- Discover live agent sessions across all known terminal instances.
- Bind panes to stable session keys shaped as `harness:native-session-id`.
- Recover already-running sessions through terminal tags and scrollback
  fingerprinting when hooks were not present at startup.
- List live panes, raw terminal panes, sockets, and native session history.
- Query status, titles, scrollback, plans/todos, prompts, transcript tails, and
  disk identity.
- Hide/unhide noisy sessions from the default history view.
- Resume sessions by index, continue the most recent idle session, or browse
  everything through the interactive resume TUI.

### Resume TUI

- Two-pane session browser with synchronized list and transcript views.
- Persistent display preferences for transcript visibility, role filters, body
  mode, cwd mode, snipping, and token display.
- Transcript preview modes for full text, compact snips, user-focused views, and
  collapsed intermittent assistant/tool chatter.
- Independent list filtering and transcript search so a query can stay pinned
  while moving across sessions.
- CWD/project/touched-project columns with workgroup-aware coloring.
- Created/modified age columns, turn counts, token density, compact braille
  token display, and reversible column cycling.
- Full disk identity yank for debugging native storage provenance.
- Resume launch behavior that can either move focus into the new terminal or
  keep focus on the browser for batch work.

### Prompt And Transcript Tools

- Directory-scoped prompt history with count, time window, path, recursive,
  string filter, context-row, and token-budget context modes.
- `cat` and `tail` surfaces for pipe-friendly transcript previews.
- Native transcript parsing for Claude Code and Codex CLI, with scanner slots
  for the wider harness roster.
- Session title reads/writes through harness-owned storage where supported.
- Summarizer-backed title update surface for sessions that are not in flight.

### Terminal Orchestration

- Kitty remote-control adapter for pane listing, focus, text send/type,
  scrollback, titles, user vars, border color, and pane close.
- Socket-aware `PaneAddr` identity so panes from multiple kitty instances do not
  collide.
- Pending-input guard before sends and broadcasts.
- Pane solo mode for reducing diagnostic noise.
- Pane reboot that preserves session identity, cwd, workspace, and geometry.
- Tmux and zellij integration surfaces behind explicit backend capability,
  rather than cwd/title guessing.

### Lifecycle And State

- Registry-driven hook normalization across harnesses.
- Canonical events for session start, prompt submit, pre-tool, post-tool,
  notification, subagent stop, pre-compact, and stop.
- Direct hook push path into daemon memory to avoid polling lag.
- Activity state reducer with hook truth preferred over scrollback evidence.
- Read/unread effects and visual pulses flow into Babel state first, then panel
  paint output.

### Daemon, IPC, And Paint

- Long-running daemon for cached state, fast client commands, live subscribers,
  and panel clients.
- Warmup/readiness reporting so clients can wait for a present daemon instead
  of racing into local self-served scans.
- Stable IPC DTOs for requests, responses, event streams, and paint streams.
- `monitor` and debug TUI surfaces for daemon/event inspection.
- Typed paint stream that carries resolved window/workspace render truth:
  colors, rings, scale, outline, urgency, and workspace aggregation.

### Migration And Recovery

- Harness-aware `babel mv --doctor` evidence report for directory moves.
- Typed migration planning over live panes and native storage surfaces.
- Apply path with mutation manifests, snapshots, verification, and rollback for
  owned state.
- Claude Code and Codex CLI are the main mutation paths; weaker adapters report
  preservation hints, doctor-only state, or unsupported status honestly.
- `mv-log` exposes recent migration transaction manifests.

### Automation And Coordination

- Fire detached background agent tasks with cwd detection.
- Fork a new agent from another session with transcript context in `reflect`,
  `continue`, or `review` mode.
- Broadcast prompts across panes with safety checks.
- Workspace sets save/load multi-pane layouts.
- MCP server exposes Babel session management to Claude Code or any MCP client.
- Mermaid architecture generation helper for mapping other codebases.

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
babel ls                     # Compact live pane view
babel ls -d                  # Detailed view with fingerprint data
babel ls --history           # Current-directory session history
babel ls --history-recursive # Recursive history under cwd
babel ls --all               # Include ordinary/unrecognized terminal panes

# Native session index
babel ls-sessions            # Recent sessions across harnesses
babel ls-sessions --uuid     # Include stable native ids
babel ls-sessions --kind codex
babel hide 3 6 9
babel unhide 3

# Check specific pane status
babel get-window 42          # By pane ID
babel get-window             # Focused agent pane
babel get-scrollback . -l 80 # Current pane scrollback
babel get-title .

# Pane control
babel focus 42               # Focus pane
babel send 42 "text"         # Send text and Enter
babel type . "draft..."      # Type without submitting
babel broadcast "run checks" # Send to all agent panes
babel solo 42                # Hide other panes from Babel views
babel solo --off

# Session metadata
babel set-icon 42 "*"        # Custom icon indicator
babel set-read 42            # Mark as read
babel set-title 42           # Auto-title from session
babel set-title '*' "Title"  # Custom title

# Prompt and transcript history
babel prompts                 # Recent prompts for cwd
babel prompts 50              # Last 50 prompts
babel prompts ./subdir 2d -r  # Recursive prompt history
babel prompts --filter tui -c 2
babel cat abc123              # One-line transcript preview
babel tail . -n 50            # Last 50 transcript messages

# Resume and fork
babel resume                  # Interactive TUI browser
babel resume 1 4              # Resume by ls-sessions index
babel continue                # Resume most recent non-running session
babel fork . -m review        # New agent with transcript context

# Fire-and-forget launch
babel fire "Implement feature X"
babel fire -d ~/project "Fix the bug"
babel fire-ls
babel fire-clean

# Directory migration (preserves provider history)
babel mv --doctor ~/old ~/new # Universal evidence report
babel mv ~/old ~/new          # Apply supported typed migration
babel mv-log                  # Recent migration manifests

# Workspace sets
babel wset save mysetup
babel wset load mysetup --dry
babel wset ls

# Daemon, hooks, integrations
babel daemon                  # Start daemon
babel monitor                 # Stream daemon events
babel tui                     # Interactive debug console
babel hook install --dry-run  # Preview hook wiring
babel mcp                     # MCP stdio server
babel tmux-setup              # Print tmux integration snippet
```

## Architecture

### Modules

- **agent_kind**: Harness roster, accent identity, and support classification.
- **backend**: Terminal abstraction with kitty as the reference adapter and
  explicit tmux/zellij capability surfaces.
- **babel_storage**: Babel-owned sqlite state for overlay metadata, hook state,
  read/unread state, migration transactions, and cached session facts.
- **core**: Client-facing orchestration over daemon IPC or local fallback paths.
- **daemon** / **service**: Long-running refresh, matching, activity reduction,
  paint publication, and IPC serving.
- **harness**: Per-harness native session, transcript, title, hook, and resume
  contracts.
- **harness_ops**: Migration planning/apply/report machinery over native storage
  and live panes.
- **ipc** / **events** / **paint**: Stable wire DTOs for clients, monitors, and
  panel renderers.
- **pager**: Resume TUI state, session list rendering, transcript rendering,
  preferences, touched-project metrics, and disk identity yanking.
- **title_policy**: Session title buffering, rolling prompts, and splice helpers.
- **utility**: Discovery, workdir, storage, and IPC helpers.
- **wset**: Workspace set capture/restore.

### Data Storage

- **Provider-native storage**: Managed by each harness and treated as source of
  truth. Babel reads and mutates only through adapter-owned contracts.
  - Claude Code: `~/.claude/`
  - Codex CLI: `~/.codex/`
  - Other harnesses: documented per adapter when the storage surface is known.

- **Babel state**: Managed by Babel under local config/data/state roots.
  - Overlay metadata: icons, read state, notes, hook state.
  - Refresh/cache state: session facts that can be rebuilt.
  - Migration transactions: manifests, snapshots, verification reports.
  - Resume preferences and workspace sets.

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

## Operational Surfaces

### Terminal Protocol

Kitty is the only implemented terminal adapter today. Babel uses kitty remote
control for pane/window state, input, focus, scrollback, titles, user vars,
border color, and cross-instance routing. Any future terminal adapter must
expose equivalent pane identity and control primitives before it can be trusted
as an orchestration backend.

### Native Harness Storage

Harness modules own provider-specific formats. Feature code should ask the
harness layer for sessions, transcripts, resume commands, titles, and migration
edits instead of hardcoding Claude/Codex file shapes in pager or daemon code.

### Hooks And Bridges

Direct hook integrations call `babel hook ...` or pipe JSON into
`babel hook stdin <event> --agent <slug>`. Bridge-only harnesses must adapt
their callback/plugin system to the same canonical events rather than forcing
Babel to guess lifecycle state from cwd, title, or wall-clock timing.

### Panels And Monitors

External clients should prefer `SubscribePaint` over re-deriving state from raw
events. The paint stream is the UX contract: Babel resolves state, color,
intensity, outline, and aggregation; clients render it.

## Current Status

Babel is past the old "Claude helper" stage. The current center is:

- Claude Code and Codex CLI as daily-driver harnesses.
- Kitty as the reference terminal backend.
- Daemon-backed live state with readiness-aware clients.
- Resume TUI as the session command center.
- `babel mv` as real harness-aware migration infrastructure.
- Typed paint stream as the panel integration contract.
- MCP, tmux, and zellij surfaces as integration frontiers.

Active frontier work should stay in these lanes:

- tighten daemon startup/warmup latency and traces;
- keep resume list/transcript state perfectly synchronized across modes;
- improve touched-project/workgroup coloring and filtering;
- expand title summarization only through harness-safe mutation paths;
- promote tmux/zellij only when their adapters prove stable identity and
  control context;
- keep migration verification exact, post-mutation, and native-storage-backed.

## Design Notes

### Separation Of Concerns

Babel does not make provider files its private database. Provider-native storage
remains the source of truth for transcripts and resume identity. Babel-owned
state holds overlay data, read state, hook bindings, display preferences,
migration manifests, and rebuildable caches.

### No Guessing

The orchestration bar is stable identity plus lifecycle signal. If a harness
lacks that, Babel may document it, probe it, or show the terminal pane, but it
should not pretend cwd/time/title heuristics are durable session truth.

### Performance

- Session and transcript scans are streamed and cached where possible.
- Daemon refresh owns expensive pane/session matching.
- Clients wait for daemon readiness when a daemon is present.
- Panel consumers receive paint deltas instead of polling native storage.
- Indexes may accelerate search, but source-of-truth data must remain
  rebuildable from native storage.

### Trace-Driven Development

When state looks wrong, add transition-gated traces at the reducer, matching,
hook, IPC, or render boundary. The useful trace is not "more logs everywhere";
it is a compact record of the state transition, inputs, selected policy, and
output identity at the moment the system chooses a path.

## License

Personal use - part of nuck's dotfiles workspace
