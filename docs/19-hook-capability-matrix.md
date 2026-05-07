# Hook Capability Matrix

Babel normalizes harness lifecycle hooks into **13 canonical events**
(up from 8 in the initial release). Each harness emits some subset
under its own native event names; Babel's roster (`events.rs`) maps
native -> canonical so the daemon, paint stream, and storage layer
never think in provider terms.

This document tracks two dimensions:

1. **Harness -> canonical event coverage** (what Babel's roster knows)
2. **Shipped config -> actually wired** (what `hooks/*` registers)

## Canonical Events

| #  | Canonical        | State Effect | Read Effect | Pulse     | Purpose                                 |
|----|------------------|-------------|-------------|-----------|------------------------------------------|
| 1  | `session-start`  | Working     | Preserve    | Session   | Harness session begins / resumes         |
| 2  | `prompt`         | Working     | MarkRead    | Prompt    | User submits a prompt                    |
| 3  | `pre-tool`       | ToolRunning | Preserve    | Tool      | Tool is about to execute                 |
| 4  | `post-tool`      | Working     | Preserve    | Tool      | Tool finished executing                  |
| 5  | `stop`           | Idle        | MarkUnread  | Finished  | Agent turn ends cleanly                  |
| 6  | `notification`   | ---         | Preserve    | Attention | System notification / info event         |
| 7  | `subagent-stop`  | ---         | Preserve    | Finished  | Subagent finished (CC-family)            |
| 8  | `pre-compact`    | ---         | Preserve    | Compact   | Context compaction about to fire         |
| 9  | `session-end`    | Idle        | Preserve    | Teardown  | Session teardown / harness exit          |
| 10 | `subagent-start` | ---         | Preserve    | Session   | Subagent spawned                         |
| 11 | `stop-failure`   | Idle        | MarkUnread  | Error     | Turn ended due to error (API, rate, auth)|
| 12 | `post-compact`   | ---         | Preserve    | Compact   | After compaction completes               |
| 13 | `permission`     | ---         | Preserve    | Attention | Permission dialog shown to user          |

Events 1--8 are the original set. Events 9--13 were added in the
second wave to close observability gaps around session lifecycle,
error states, and compaction bookends.

## Event Arrays by Harness Family

Each harness family defines a fixed-size event array. CC-family
harnesses share the 13-event Claude Code array regardless of their
native event naming.

| Family       | Size | Members                                                                  |
|--------------|------|--------------------------------------------------------------------------|
| CLAUDE_CODE  | 13   | Claude, QwenCode, Kimi, FactoryDroid, Cursor, Amp\*, OpenCode\*, Kiro\* |
| CODEX        | 6    | SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, PermissionRequest |
| GEMINI       | 6    | SessionStart(new), BeforeTool, AfterTool, BeforeAgent, Stop, PreCompress |
| CLINE        | 6    | TaskStart, UserPromptSubmit, PreToolUse, PostToolUse, TaskComplete, Notification(new) |
| CRUSH        | 1    | PreToolUse                                                               |

\* = BridgeRequired (roster mapping exists but no shipped config; bridge adapter needed)

## Harness x Canonical Event Matrix

Legend:
- `+` = in roster AND wired in shipped config
- `R` = roster-only (no shipped config -- bridge harnesses)
- `---` = not in roster (harness doesn't emit this)
- `x` = unsupported harness (no hook surface)

| Harness            | Support              | Install          | ses-start | prompt | pre-tool | post-tool | stop | notif | subag-stop | pre-compact | ses-end | subag-start | stop-fail | post-compact | permiss |
|--------------------|----------------------|------------------|-----------|--------|----------|-----------|------|-------|------------|-------------|---------|-------------|-----------|--------------|---------|
| **Claude**         | Supported            | AutoJsonSettings | +         | +      | +        | +         | +    | +     | +          | +           | +       | +           | +         | +            | +       |
| **Codex**          | Supported            | JsonSnippet      | +         | +      | +        | +         | +    | ---   | ---        | ---         | ---     | ---         | ---       | ---          | +       |
| **FactoryDroid**   | Supported            | JsonSnippet      | +         | +      | +        | +         | +    | +     | +          | +           | +       | +           | +         | +            | +       |
| **QwenCode**       | Supported            | JsonSnippet      | +         | +      | +        | +         | +    | +     | +          | +           | +       | +           | +         | +            | +       |
| **Gemini**         | Supported            | JsonSnippet      | +         | ---    | +        | +         | +    | ---   | ---        | +           | ---     | ---         | ---       | ---          | ---     |
| **Crush**          | Supported            | JsonSnippet      | ---       | ---    | +        | ---       | ---  | ---   | ---        | ---         | ---     | ---         | ---       | ---          | ---     |
| **Cursor**         | Supported            | JsonSnippet      | +         | +      | +        | +         | +    | +     | +          | +           | +       | +           | +         | +            | +       |
| **Kimi**           | Supported            | TomlSnippet      | +         | +      | +        | +         | +    | +     | +          | +           | +       | +           | +         | +            | +       |
| **Cline**          | Supported            | FsSnippet        | +         | +      | +        | +         | +    | +     | ---        | ---         | ---     | ---         | ---       | ---          | ---     |
| **OpenCode**       | BridgeRequired       | BridgeContract   | R         | R      | R        | R         | R    | R     | R          | R           | R       | R           | R         | R            | R       |
| **Amp**            | BridgeRequired       | BridgeContract   | R         | R      | R        | R         | R    | R     | R          | R           | R       | R           | R         | R            | R       |
| **Kiro**           | BridgeRequired       | BridgeContract   | R         | R      | R        | R         | R    | R     | R          | R           | R       | R           | R         | R            | R       |
| **GitHub Copilot** | Unsupported          | ---              | x         | x      | x        | x         | x    | x     | x          | x           | x       | x           | x         | x            | x       |
| **RooCode**        | Unsupported          | ---              | x         | x      | x        | x         | x    | x     | x          | x           | x       | x           | x         | x            | x       |
| **KiloCode**       | Unsupported          | ---              | x         | x      | x        | x         | x    | x     | x          | x           | x       | x           | x         | x            | x       |
| **Aider**          | Unsupported          | ---              | x         | x      | x        | x         | x    | x     | x          | x           | x       | x           | x         | x            | x       |
| **Antigravity**    | Unsupported          | ---              | x         | x      | x        | x         | x    | x     | x          | x           | x       | x           | x         | x            | x       |

### Support Tiers

| Tier                          | Meaning                                                       |
|-------------------------------|---------------------------------------------------------------|
| Supported + AutoJsonSettings  | Babel auto-installs hooks into harness settings.json          |
| Supported + JsonSnippet       | User copies shipped JSON config into harness settings         |
| Supported + TomlSnippet       | User copies shipped TOML config into harness settings         |
| Supported + FsSnippet         | Filesystem-convention reference (Cline reads `.md` rules)     |
| BridgeRequired                | Roster mapping exists; needs bridge adapter to actually fire  |
| Unsupported                   | No hook surface exposed by the harness                        |

### Shipped Configs

| File                | Format | Family      | Events |
|---------------------|--------|-------------|--------|
| `hooks/claude.json` | JSON   | CLAUDE_CODE | 13/13  |
| `hooks/codex.json`  | JSON   | CODEX       | 6/6    |
| `hooks/factory-droid.json` | JSON | CLAUDE_CODE | 13/13 |
| `hooks/qwen-code.json` | JSON | CLAUDE_CODE | 13/13  |
| `hooks/cursor.json` | JSON   | CLAUDE_CODE | 13/13  |
| `hooks/gemini.json` | JSON   | GEMINI      | 6/6    |
| `hooks/crush.json`  | JSON   | CRUSH       | 1/1    |
| `hooks/kimi.toml`   | TOML   | CLAUDE_CODE | 13/13  |
| `hooks/cline.md`    | FS ref | CLINE       | 6/6    |

All shipped configs are now at full coverage for their respective
event arrays. No config gaps remain.

## Claude Code: Full Spec vs Babel Coverage

CC emits ~29 hook event types. Babel maps 13 of them (up from 8).
The remaining 16 unmapped events are listed below as expansion
candidates, roughly ordered by value.

| CC Event             | Babel Canonical  | Status         | Notes                                         |
|----------------------|------------------|----------------|-----------------------------------------------|
| SessionStart         | session-start    | **Mapped**     |                                               |
| UserPromptSubmit     | prompt           | **Mapped**     |                                               |
| PreToolUse           | pre-tool         | **Mapped**     |                                               |
| PostToolUse          | post-tool        | **Mapped**     |                                               |
| Stop                 | stop             | **Mapped**     |                                               |
| Notification         | notification     | **Mapped**     |                                               |
| SubagentStop         | subagent-stop    | **Mapped**     |                                               |
| PreCompact           | pre-compact      | **Mapped**     |                                               |
| SessionEnd           | session-end      | **Mapped**     | New -- drives cleanup without timeout          |
| SubagentStart        | subagent-start   | **Mapped**     | New -- tracks parallel worker spawns           |
| StopFailure          | stop-failure     | **Mapped**     | New -- distinguishes clean stop from error     |
| PostCompact          | post-compact     | **Mapped**     | New -- compaction duration bookend             |
| PermissionRequest    | permission       | **Mapped**     | New -- permission UX awareness                 |
| Setup                | ---              | Not mapped     | `--init-only` / maintenance mode               |
| UserPromptExpansion  | ---              | Not mapped     | Slash command expansion                        |
| InstructionsLoaded   | ---              | Not mapped     | CLAUDE.md loaded into context                  |
| PostToolUseFailure   | ---              | Not mapped     | CC splits success/failure; babel merges via post-tool |
| PostToolBatch        | ---              | Not mapped     | All parallel tool calls resolved               |
| PermissionDenied     | ---              | Not mapped     | Auto-mode classifier denied a tool             |
| Elicitation          | ---              | Not mapped     | MCP server requests user input                 |
| ElicitationResult    | ---              | Not mapped     | User responded to elicitation                  |
| TeammateIdle         | ---              | Not mapped     | Team agent going idle                          |
| TaskCreated          | ---              | Not mapped     | TaskCreate tool call                           |
| TaskCompleted        | ---              | Not mapped     | Task marked completed                          |
| ConfigChange         | ---              | Not mapped     | Settings file changed during session           |
| CwdChanged           | ---              | Not mapped     | Working directory changed                      |
| FileChanged          | ---              | Not mapped     | Watched file changed on disk                   |
| WorktreeCreate       | ---              | Not mapped     | Worktree being created                         |
| WorktreeRemove       | ---              | Not mapped     | Worktree being removed                         |

## Codex CLI: Full Spec vs Babel Coverage

Codex has 6 event types plus a separate `notify` mechanism.
All 6 are now mapped and wired.

| Codex Event       | Babel Canonical | Status      | Notes                                             |
|-------------------|-----------------|-------------|---------------------------------------------------|
| SessionStart      | session-start   | **Mapped**  |                                                   |
| UserPromptSubmit  | prompt          | **Mapped**  |                                                   |
| PreToolUse        | pre-tool        | **Mapped**  |                                                   |
| PostToolUse       | post-tool       | **Mapped**  |                                                   |
| Stop              | stop            | **Mapped**  |                                                   |
| PermissionRequest | permission      | **Mapped**  |                                                   |
| *(notify)*        | *(legacy)*      | Separate    | `agent-turn-complete` via `handle_codex_notify`   |

## Gemini CLI: Full Spec vs Babel Coverage

Gemini has 6 event types. SessionStart is new to the roster.

| Gemini Event  | Babel Canonical | Status     | Notes                                          |
|---------------|-----------------|------------|-------------------------------------------------|
| SessionStart  | session-start   | **Mapped** | New addition                                    |
| BeforeTool    | pre-tool        | **Mapped** |                                                 |
| AfterTool     | post-tool       | **Mapped** |                                                 |
| BeforeAgent   | prompt          | **Mapped** | Gemini's equivalent to prompt; mapped to stop   |
| Stop          | stop            | **Mapped** |                                                 |
| PreCompress   | pre-compact     | **Mapped** | Gemini's name for compaction                    |

## Expansion Priorities

The 5 highest-value unmapped CC events for future waves:

1. **PostToolBatch** --- aggregate signal after parallel tool calls resolve;
   useful for batch-tool-aware paint states
2. **PostToolUseFailure** --- CC splits success/failure explicitly; babel
   currently merges both into `post-tool`, but a dedicated `tool-failure`
   canonical could drive error-specific paint
3. **PermissionDenied** --- paired with `permission`, completes the
   permission lifecycle for UX tracking
4. **TaskCreated / TaskCompleted** --- task lifecycle for multi-agent
   orchestration visibility
5. **Elicitation / ElicitationResult** --- MCP elicitation lifecycle;
   relevant as MCP adoption grows
