# Harness Adapter Matrix

## Harness: Claude Code

Readiness: `doctor-only`.

Storage facts:

- `~/.claude/projects/<project-key>/*.jsonl`
- `~/.claude/history.jsonl`
- `~/.claude.json`
- `~/.claude/settings.json`
- `~/.claude/todos`
- `~/.claude/tasks`
- plugin and usage metadata under `~/.claude`

Apply requirement: copy/verify/rewrite/rollback across the full state set, not
only project transcripts.

## Harness: Codex CLI

Readiness: `apply-ready` for typed JSONL, TOML, text-ref, and SQLite
text-column rewrites.

Storage facts:

- `~/.codex/sessions/**/*.jsonl`
- `~/.codex/history.jsonl`
- `~/.codex/config.toml`
- `~/.codex/session_index.jsonl`
- `~/.codex/shell_snapshots`
- `~/.codex/state_*.sqlite`

Required move operations:

- rewrite `session_meta.payload.cwd` in rollout JSONL
- rewrite path references in rollout/history text surfaces
- rewrite `[projects."<cwd>"]` keys in `config.toml`
- rewrite `state_*.sqlite:threads.cwd`
- preserve shell snapshots keyed by session id

## Harness: Gemini CLI

Readiness: `doctor-only`.

Storage facts:

- `~/.gemini/tmp/<project-id>/chats`
- optional `~/.gemini/projects.json`
- older installs may expose `~/.gemini/sessions`

Open point: project identity is hash/path based; apply needs destination hash
and fixture-backed migration rules.

## Harness: Qwen Code

Readiness: `doctor-only`.

Storage facts:

- `~/.qwen/projects/<sanitized-cwd>/chats/*.jsonl`
- observed `~/.qwen/tmp/<project>/config.json`

Open point: Qwen has compatible runtime identity, but path-move storage rules
still need native fixtures.

## Harness: Cursor Agent

Readiness: `doctor-only`.

Storage facts:

- `~/.config/Cursor/User/globalStorage/state.vscdb`
- `~/.config/Cursor/User/workspaceStorage/<hash>/state.vscdb`
- `~/.config/Cursor/User/workspaceStorage/<hash>/workspace.json`
- legacy `~/.cursor/projects`

Apply requirement: close Cursor, back up SQLite databases/images, mutate through
SQLite-aware transactions, verify read-back, then restart.

## Harness: Cline, Roo Code, Kilo Code

Readiness: `doctor-only` or `recon-only` depending on detected roots.

Storage facts:

- VS Code-family extension global storage
- task folders containing `ui_messages.json`, `api_conversation_history.json`,
  and task metadata sidecars

Apply requirement: fixture-backed per-extension path semantics and IDE shutdown
discipline.

## Harness: OpenCode

Readiness: `doctor-only`.

Storage facts:

- current SQLite under XDG data roots
- legacy JSON under `storage/{session,message,part}`
- plugin model is in-process, not stdin/stdout hooks

Apply requirement: SQLite mutation contract and process-lock refusal.

## Harness: Amp

Readiness: `doctor-only`.

Storage facts:

- `~/.local/share/amp`
- `~/.config/amp`
- thread JSON observed by references

Apply requirement: fixture-backed JSON rewrite and process-lock rules.

## Harness: Aider

Readiness: preservation hint.

Storage facts:

- project-local `.aider*` files

Move usually preserves state by moving the project directory itself.

