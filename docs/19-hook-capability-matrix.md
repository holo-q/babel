# Hook Capability Matrix

Babel normalizes harness lifecycle hooks into 8 **canonical events**.
Each harness emits some subset under its own native event names;
Babel's roster (`events.rs`) maps native → canonical so the daemon,
paint stream, and storage layer never think in provider terms.

This document tracks two dimensions:

1. **Harness → canonical event coverage** (what Babel's roster knows)
2. **Shipped config → actually wired** (what `hooks/*.json` registers)

## Canonical Events

| Canonical       | State Effect | Read Effect | Pulse     | Purpose                                      |
|-----------------|-------------|-------------|-----------|----------------------------------------------|
| `session-start` | Working     | Preserve    | Session   | Harness session begins / resumes             |
| `prompt`        | Working     | MarkRead    | Prompt    | User submits a prompt                        |
| `pre-tool`      | ToolRunning | Preserve    | Tool      | Tool is about to execute                     |
| `post-tool`     | Working     | Preserve    | Tool      | Tool finished executing                      |
| `stop`          | Idle        | MarkUnread  | Finished  | Agent turn ends                              |
| `notification`  | —           | Preserve    | Attention | System notification / permission prompt      |
| `subagent-stop` | —           | Preserve    | Finished  | Subagent finished (Claude Code only)         |
| `pre-compact`   | —           | Preserve    | Compact   | Context compaction about to fire              |

## Harness × Canonical Event Matrix

Rows: harnesses. Columns: canonical events.
- `✓` = in roster (`events.rs`) AND wired in shipped config
- `R` = in roster only (handler code exists, config not shipped)
- `—` = not in roster (harness doesn't emit this)
- `✗` = unsupported harness (no hook surface at all)

| Harness          | Support   | Install          | session | prompt | pre-tool | post-tool | stop | notif | subagent | compact |
|------------------|-----------|------------------|---------|--------|----------|-----------|------|-------|----------|---------|
| **Claude Code**  | Supported | AutoJsonSettings | ✓       | ✓      | R        | ✓         | ✓    | ✓     | ✓        | ✓       |
| **Codex**        | Supported | JsonSnippet      | ✓       | ✓      | R        | ✓         | ✓    | R     | —        | —       |
| **Factory Droid**| Supported | JsonSnippet      | R       | R      | R        | R         | R    | R     | R        | R       |
| **Qwen Code**    | Supported | JsonSnippet      | R       | R      | R        | R         | R    | R     | R        | R       |
| **Kimi**         | Supported | TomlSnippet      | R       | R      | R        | R         | R    | R     | R        | R       |
| **Gemini**       | Supported | JsonSnippet      | —       | R      | R        | R         | R    | —     | —        | R       |
| **Crush**        | Supported | JsonSnippet      | —       | —      | R        | —         | —    | —     | —        | —       |
| **Cursor**       | Supported | JsonSnippet      | R       | R      | R        | R         | R    | R     | R        | R       |
| **Cline**        | Supported | FsSnippet        | R       | R      | R        | R         | R    | —     | —        | —       |
| **OpenCode**     | Bridge    | BridgeContract   | R       | R      | R        | R         | R    | R     | R        | R       |
| **Amp**          | Bridge    | BridgeContract   | R       | R      | R        | R         | R    | R     | R        | R       |
| **Kiro**         | Bridge    | BridgeContract   | R       | R      | R        | R         | R    | R     | R        | R       |
| **GitHub Copilot** | ✗       | —                | ✗       | ✗      | ✗        | ✗         | ✗    | ✗     | ✗        | ✗       |
| **Roo Code**     | ✗         | —                | ✗       | ✗      | ✗        | ✗         | ✗    | ✗     | ✗        | ✗       |
| **Kilo Code**    | ✗         | —                | ✗       | ✗      | ✗        | ✗         | ✗    | ✗     | ✗        | ✗       |
| **Aider**        | ✗         | —                | ✗       | ✗      | ✗        | ✗         | ✗    | ✗     | ✗        | ✗       |
| **Antigravity**  | ✗         | —                | ✗       | ✗      | ✗        | ✗         | ✗    | ✗     | ✗        | ✗       |

## Claude Code: Full Spec vs Babel Coverage

Claude Code emits ~29 hook event types. Babel maps 8 of them.
The unmapped events are listed here as expansion candidates.

| CC Event              | Babel Canonical | Status       | Notes                                    |
|-----------------------|----------------|--------------|------------------------------------------|
| SessionStart          | session-start  | **Wired**    |                                          |
| UserPromptSubmit      | prompt         | **Wired**    |                                          |
| PreToolUse            | pre-tool       | **Roster**   | Handler exists. Missing from `claude.json` |
| PostToolUse           | post-tool      | **Wired**    |                                          |
| Stop                  | stop           | **Wired**    |                                          |
| Notification          | notification   | **Wired**    |                                          |
| SubagentStop          | subagent-stop  | **Wired**    |                                          |
| PreCompact            | pre-compact    | **Wired**    |                                          |
| SessionEnd            | —              | Not mapped   | Session teardown; could drive cleanup    |
| Setup                 | —              | Not mapped   | `--init-only` / maintenance mode         |
| UserPromptExpansion   | —              | Not mapped   | Slash command expansion                  |
| InstructionsLoaded    | —              | Not mapped   | CLAUDE.md loaded into context            |
| PostToolUseFailure    | —              | Not mapped   | Tool failure (CC splits success/failure) |
| PostToolBatch         | —              | Not mapped   | All parallel tool calls resolved         |
| PermissionRequest     | —              | Not mapped   | Permission dialog shown                  |
| PermissionDenied      | —              | Not mapped   | Auto-mode classifier denied a tool       |
| Elicitation           | —              | Not mapped   | MCP server requests user input           |
| ElicitationResult     | —              | Not mapped   | User responded to elicitation            |
| SubagentStart         | —              | Not mapped   | Subagent spawned                         |
| StopFailure           | —              | Not mapped   | Turn ended due to API error              |
| TeammateIdle          | —              | Not mapped   | Team agent going idle                    |
| TaskCreated           | —              | Not mapped   | TaskCreate tool call                     |
| TaskCompleted         | —              | Not mapped   | Task marked completed                    |
| PreCompact            | pre-compact    | **Wired**    |                                          |
| PostCompact           | —              | Not mapped   | After compaction completes               |
| ConfigChange          | —              | Not mapped   | Settings file changed during session     |
| CwdChanged            | —              | Not mapped   | Working directory changed                |
| FileChanged           | —              | Not mapped   | Watched file changed on disk             |
| WorktreeCreate        | —              | Not mapped   | Worktree being created                   |
| WorktreeRemove        | —              | Not mapped   | Worktree being removed                   |

## Codex CLI: Full Spec vs Babel Coverage

Codex has 6 event types plus a separate `notify` mechanism.

| Codex Event       | Babel Canonical | Status       | Notes                                     |
|-------------------|----------------|--------------|-------------------------------------------|
| SessionStart      | session-start  | **Wired**    |                                           |
| UserPromptSubmit  | prompt         | **Wired**    |                                           |
| PreToolUse        | pre-tool       | **Roster**   | Missing from `codex.json`                 |
| PostToolUse       | post-tool      | **Wired**    |                                           |
| Stop              | stop           | **Wired**    |                                           |
| PermissionRequest | notification   | **Roster**   | Missing from `codex.json`                 |
| *(notify)*        | *(legacy)*     | Separate     | `agent-turn-complete` via `handle_codex_notify` |

## Shipped Config Gaps

These are events where the roster + handler code exist but the config file doesn't register them.
Fixing these is a one-line addition to each JSON file.

| Config File    | Missing Event       | Native Name       | Canonical  |
|----------------|--------------------|--------------------|------------|
| `claude.json`  | PreToolUse         | `PreToolUse`       | pre-tool   |
| `codex.json`   | PreToolUse         | `PreToolUse`       | pre-tool   |
| `codex.json`   | PermissionRequest  | `PermissionRequest`| notification |

## Expansion Priorities

High-value unmapped CC events for Babel's use cases:

1. **SessionEnd** — cleanup pane tags, mark session dead without timeout
2. **SubagentStart** — track parallel worker spawns, could show agent tree
3. **StopFailure** — distinguish clean stop from error (rate limit, auth)
4. **PostCompact** — pair with PreCompact for compaction duration tracking
5. **PermissionRequest** — already mapped for Codex; CC version adds permission UX awareness
