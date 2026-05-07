# Hook Capability Matrix

Babel normalizes harness lifecycle hooks into **29 canonical events**.
Each harness emits some subset under its own native event names;
Babel's roster (`events.rs`) maps native -> canonical so the daemon,
paint stream, and storage layer never think in provider terms.

This document is the reference for:

1. **All 29 canonical events** -- their effects on state, read, and pulse
2. **Harness family event arrays** -- size and membership
3. **Harness x event coverage** -- what's wired, what's roster-only
4. **Shipped configs** -- what's in the `hooks/` directory
5. **PulseEffect visual design** -- how each pulse drives the indicator

## Canonical Events -- 29 CC + harness variants

The CLAUDE_CODE array has 29 entries. All CC-family harnesses
share this full array. Other families (Codex, Gemini, Cline, Crush)
map their native events into the same canonical namespace at smaller
array sizes.

| #  | Native CC Event       | Canonical           | State       | Read        | Pulse     |
|----|-----------------------|---------------------|-------------|-------------|-----------|
| 1  | SessionStart          | `session-start`     | Working     | Preserve    | Session   |
| 2  | UserPromptSubmit      | `prompt`            | Working     | MarkRead    | Prompt    |
| 3  | PreToolUse            | `pre-tool`          | ToolRunning | Preserve    | Tool      |
| 4  | PostToolUse           | `post-tool`         | Working     | Preserve    | Tool      |
| 5  | Stop                  | `stop`              | Idle        | MarkUnread  | Finished  |
| 6  | Notification          | `notification`      | --          | Preserve    | Attention |
| 7  | SubagentStop          | `subagent-stop`     | --          | Preserve    | Finished  |
| 8  | PreCompact            | `pre-compact`       | --          | Preserve    | Compact   |
| 9  | SessionEnd            | `session-end`       | Idle        | Preserve    | Teardown  |
| 10 | SubagentStart         | `subagent-start`    | --          | Preserve    | Session   |
| 11 | StopFailure           | `stop-failure`      | Idle        | MarkUnread  | Error     |
| 12 | PostCompact           | `post-compact`      | --          | Preserve    | Compact   |
| 13 | PermissionRequest     | `permission`        | --          | Preserve    | Attention |
| 14 | Setup                 | `setup`             | --          | Preserve    | Session   |
| 15 | UserPromptExpansion   | `prompt-expand`     | --          | Preserve    | Prompt    |
| 16 | PermissionDenied      | `permission-denied` | --          | Preserve    | Attention |
| 17 | PostToolUseFailure    | `post-tool-fail`    | Working     | Preserve    | Error     |
| 18 | PostToolBatch         | `post-tool-batch`   | Working     | Preserve    | Tool      |
| 19 | TaskCreated           | `task-created`      | --          | Preserve    | Session   |
| 20 | TaskCompleted         | `task-completed`    | --          | Preserve    | Finished  |
| 21 | TeammateIdle          | `teammate-idle`     | --          | Preserve    | Attention |
| 22 | InstructionsLoaded    | `instructions-loaded` | --        | Preserve    | None      |
| 23 | ConfigChange          | `config-change`     | --          | Preserve    | Attention |
| 24 | CwdChanged            | `cwd-changed`       | --          | Preserve    | None      |
| 25 | FileChanged           | `file-changed`      | --          | Preserve    | None      |
| 26 | WorktreeCreate        | `worktree-create`   | --          | Preserve    | Session   |
| 27 | WorktreeRemove        | `worktree-remove`   | --          | Preserve    | Teardown  |
| 28 | Elicitation           | `elicitation`       | --          | Preserve    | Attention |
| 29 | ElicitationResult     | `elicitation-result` | --         | Preserve    | None      |

Events 1--8 are the original set. Events 9--13 closed observability
gaps (session lifecycle, errors, compaction bookends). Events 14--29
complete full coverage of every known Claude Code hook event type.

## Event Arrays by Harness Family

| Family      | Size | Harnesses                                                                |
|-------------|------|--------------------------------------------------------------------------|
| CLAUDE_CODE | 29   | Claude, QwenCode, Kimi, FactoryDroid, Cursor, Amp\*, OpenCode\*, Kiro\* |
| CODEX       | 6    | Codex                                                                    |
| GEMINI      | 6    | Gemini                                                                   |
| CLINE       | 6    | Cline                                                                    |
| CRUSH       | 1    | Crush                                                                    |

\* = BridgeRequired (roster mapping exists but no shipped config; bridge adapter needed)

## Harness x Canonical Event Matrix

Legend:
- `+` = in roster AND wired in shipped config
- `R` = roster-only (bridge harnesses -- mapping exists, no shipped config)
- `--` = harness doesn't emit this event
- `x` = unsupported harness (no hook surface)

Due to the 29-column width, the CC-family harnesses are shown as
full-coverage rows. Non-CC harnesses list their supported canonicals.

### CC-Family Harnesses (29/29)

All CC-family harnesses cover the complete 29-event array.

| Harness          | Support        | Install          | Coverage |
|------------------|----------------|------------------|----------|
| **Claude**       | Supported      | AutoJsonSettings | 29/29 +  |
| **FactoryDroid** | Supported      | JsonSnippet      | 29/29 +  |
| **QwenCode**     | Supported      | JsonSnippet      | 29/29 +  |
| **Cursor**       | Supported      | JsonSnippet      | 29/29 +  |
| **Kimi**         | Supported      | TomlSnippet      | 29/29 +  |
| **OpenCode**     | BridgeRequired | BridgeContract   | 29/29 R  |
| **Amp**          | BridgeRequired | BridgeContract   | 29/29 R  |
| **Kiro**         | BridgeRequired | BridgeContract   | 29/29 R  |

### Non-CC Harnesses

| Harness          | Support     | Install     | Canonical events covered                                                    |
|------------------|-------------|-------------|-----------------------------------------------------------------------------|
| **Codex**        | Supported   | JsonSnippet | session-start, prompt, pre-tool, post-tool, stop, permission (6/6)          |
| **Gemini**       | Supported   | JsonSnippet | session-start, pre-tool, post-tool, prompt, stop, pre-compact (6/6)         |
| **Cline**        | Supported   | FsSnippet   | session-start, prompt, pre-tool, post-tool, stop, notification (6/6)        |
| **Crush**        | Supported   | JsonSnippet | pre-tool (1/1)                                                              |

### Unsupported Harnesses

| Harness            | Status      |
|--------------------|-------------|
| **GitHub Copilot** | Unsupported |
| **RooCode**        | Unsupported |
| **KiloCode**       | Unsupported |
| **Aider**          | Unsupported |
| **Antigravity**    | Unsupported |

No hook surface exposed by these harnesses.

## Support Tiers

| Tier                         | Meaning                                                      |
|------------------------------|--------------------------------------------------------------|
| Supported + AutoJsonSettings | Babel auto-installs hooks into harness settings.json         |
| Supported + JsonSnippet      | User copies shipped JSON config into harness settings        |
| Supported + TomlSnippet      | User copies shipped TOML config into harness settings        |
| Supported + FsSnippet        | Filesystem-convention reference (Cline reads `.md` rules)    |
| BridgeRequired               | Roster mapping exists; needs bridge adapter to actually fire |
| Unsupported                  | No hook surface exposed by the harness                       |

## Shipped Configs

| File                         | Format | Family      | Events |
|------------------------------|--------|-------------|--------|
| `hooks/claude.json`          | JSON   | CLAUDE_CODE | 29/29  |
| `hooks/factory-droid.json`   | JSON   | CLAUDE_CODE | 29/29  |
| `hooks/qwen-code.json`       | JSON   | CLAUDE_CODE | 29/29  |
| `hooks/cursor.json`          | JSON   | CLAUDE_CODE | 29/29  |
| `hooks/kimi.toml`            | TOML   | CLAUDE_CODE | 29/29  |
| `hooks/codex.json`           | JSON   | CODEX       | 6/6    |
| `hooks/gemini.json`          | JSON   | GEMINI      | 6/6    |
| `hooks/crush.json`           | JSON   | CRUSH       | 1/1    |
| `hooks/cline.md`             | FS ref | CLINE       | 6/6    |

All shipped configs are at full coverage for their respective
event arrays. No config gaps remain.

## PulseEffect Visual Design

Each canonical event carries a PulseEffect that drives the
indicator's visual response. The pulse determines intensity
(glow brightness), and optionally ring color and outline state.

| Pulse    | Intensity | Ring Color | Outline | Visual Meaning                    |
|----------|-----------|------------|---------|-----------------------------------|
| None     | 0.0       | --         | --      | Silent, no visual feedback        |
| Session  | 0.4       | --         | --      | Gentle glow: session lifecycle    |
| Prompt   | 0.5       | --         | --      | Moderate glow: user engaged       |
| Compact  | 0.55      | `#40c0f0`  | yes     | Sustained cyan ring: compacting   |
| Tool     | 0.65      | --         | --      | Active glow: tool running         |
| Attention| 0.75      | --         | --      | Bright glow: needs user attention |
| Error    | 0.85      | --         | --      | Hot glow: something broke         |
| Finished | 0.9       | --         | --      | Peak flash: turn complete         |
| Teardown | 0.3       | --         | --      | Dim fade: session ending          |

**Compact is special.** It has sustained ring state via the
`pane_compacting` HashSet -- when `pre-compact` fires, the ring
holds at 0.5 minimum intensity with distinct `ring_color` and
outline until `post-compact` fires and clears the pane from the
set. All other pulses are momentary bumps that decay naturally.

## Claude Code: Full Spec -- 29/29 Mapped

All 29 known Claude Code hook event types are mapped. No unmapped
events remain.

| #  | CC Native             | Babel Canonical       | Status     |
|----|-----------------------|-----------------------|------------|
| 1  | SessionStart          | session-start         | **Mapped** |
| 2  | UserPromptSubmit      | prompt                | **Mapped** |
| 3  | PreToolUse            | pre-tool              | **Mapped** |
| 4  | PostToolUse           | post-tool             | **Mapped** |
| 5  | Stop                  | stop                  | **Mapped** |
| 6  | Notification          | notification          | **Mapped** |
| 7  | SubagentStop          | subagent-stop         | **Mapped** |
| 8  | PreCompact            | pre-compact           | **Mapped** |
| 9  | SessionEnd            | session-end           | **Mapped** |
| 10 | SubagentStart         | subagent-start        | **Mapped** |
| 11 | StopFailure           | stop-failure          | **Mapped** |
| 12 | PostCompact           | post-compact          | **Mapped** |
| 13 | PermissionRequest     | permission            | **Mapped** |
| 14 | Setup                 | setup                 | **Mapped** |
| 15 | UserPromptExpansion   | prompt-expand         | **Mapped** |
| 16 | PermissionDenied      | permission-denied     | **Mapped** |
| 17 | PostToolUseFailure    | post-tool-fail        | **Mapped** |
| 18 | PostToolBatch         | post-tool-batch       | **Mapped** |
| 19 | TaskCreated           | task-created          | **Mapped** |
| 20 | TaskCompleted         | task-completed        | **Mapped** |
| 21 | TeammateIdle          | teammate-idle         | **Mapped** |
| 22 | InstructionsLoaded    | instructions-loaded   | **Mapped** |
| 23 | ConfigChange          | config-change         | **Mapped** |
| 24 | CwdChanged            | cwd-changed           | **Mapped** |
| 25 | FileChanged           | file-changed          | **Mapped** |
| 26 | WorktreeCreate        | worktree-create       | **Mapped** |
| 27 | WorktreeRemove        | worktree-remove       | **Mapped** |
| 28 | Elicitation           | elicitation           | **Mapped** |
| 29 | ElicitationResult     | elicitation-result    | **Mapped** |

## Codex CLI: 6/6 Mapped (Confirmed Complete)

Verified against `codex-rs/protocol/src/protocol.rs` (`HookEventName` enum)
and the official docs. The enum has exactly 6 variants — no expansion has
occurred. PreCompact/PostCompact is an open feature request (GitHub #17148,
2026-04-08, unimplemented).

| Codex Event       | Babel Canonical | Matcher? | Can Block? | Notes                          |
|-------------------|-----------------|----------|------------|--------------------------------|
| SessionStart      | session-start   | yes      | yes        | `source` field: startup/resume/clear |
| UserPromptSubmit  | prompt          | no       | yes        | Matcher ignored by Codex       |
| PreToolUse        | pre-tool        | yes      | yes        | `apply_patch` aliases Write/Edit |
| PermissionRequest | notification    | yes      | yes        | Fires AFTER PreToolUse, before approval UI |
| PostToolUse       | post-tool       | yes      | yes        | Includes failed commands (no separate failure event) |
| Stop              | stop            | no       | yes        | `decision:"block"` = continuation prompt |

Note: Codex PermissionRequest maps to babel canonical `notification` (not
`permission`) because it occupies the same semantic slot as CC's Notification
— a "needs attention" signal. This is intentional divergence from the CC
mapping where PermissionRequest → `permission`.

The `notify` mechanism (`config.toml` → `notify` key) is orthogonal to hooks.
Babel handles it separately via `handle_codex_notify` for the legacy
`agent-turn-complete` signal.

### What Codex Doesn't Have (confirmed absent)

No equivalent of: SessionEnd, Setup, SubagentStart/Stop, PreCompact/PostCompact,
StopFailure, Notification, PostToolUseFailure, PostToolBatch, TaskCreated/Completed,
TeammateIdle, InstructionsLoaded, ConfigChange, CwdChanged, FileChanged,
WorktreeCreate/Remove, Elicitation/ElicitationResult, UserPromptExpansion,
PermissionDenied. Codex merges tool success/failure into a single PostToolUse.

## Gemini CLI: 6/6 Mapped

| Gemini Event | Babel Canonical | Status     |
|--------------|-----------------|------------|
| SessionStart | session-start   | **Mapped** |
| BeforeTool   | pre-tool        | **Mapped** |
| AfterTool    | post-tool       | **Mapped** |
| BeforeAgent  | prompt          | **Mapped** |
| Stop         | stop            | **Mapped** |
| PreCompress  | pre-compact     | **Mapped** |

## Frontier

29/29 CC events are mapped. Codex covers its full 6/6. Gemini and
Cline cover their known event sets (6/6 each). Crush covers its
single event (1/1).

There is no remaining unmapped frontier for Claude Code. For Gemini
and Cline, additional native events may exist that Babel hasn't
discovered yet, but current coverage matches everything known.
