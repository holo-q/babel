# Harness Operations Board

This board records the prior art consumed from `references/`. The clones are
ignored local working material; this file is the durable map Babel keeps.

## Scope

Harness operations cover move, resume, handoff, archive, and recovery workflows
without turning a global search index into the source of truth. Native harness
storage remains authoritative. Any cache must be rebuildable from adapters.

`--doctor` is the shared diagnostic mode for commands in this family. It is
stronger than dry-run: it gathers evidence, reports blockers, shows exact
operation candidates, and exits without mutation.

## Move Contract

The credible apply contract comes from `cc-port` and `ccmv`, not from the old
Claude-only Babel mover:

- discover state roots and live processes before writes
- resolve paths through existing ancestors so symlinks do not produce false keys
- refuse destination key collisions before any write
- copy, verify, rewrite, and promote atomically where possible
- preserve malformed or opaque data instead of rewriting blindly
- keep backups and rollback paths for every touched root
- make interrupted/manual moves idempotent by scanning current state first

Claude storage is broader than `history.jsonl` plus `projects/<key>`. A complete
adapter must cover history, transcripts, project memory, settings, MCP/trust
metadata, todos, usage-data session metadata, usage facets, plugin data, tasks,
and opaque file-history snapshots.

## Encoding Risk

The references disagree on Claude project-key encoding details. `cc-port`
replaces `/`, `.`, and space. `ccmv` reports that every non-alphanumeric
character except `-` becomes `-`. Babel doctor probes both key shapes and reports
what actually exists instead of pretending one stale helper is truth.

## Resume And Handoff

`cli-continues`, CASR, Codbash, Chronicle, CCManager, and `cdxresume` show the
useful split:

- native resume is provider-owned
- cross-agent handoff can use canonical documents or IR
- provider-native writeback needs read-back verification
- ambiguity should produce candidates, not guesses

Babel should prefer native session ids from hooks/env/wrappers. Scrollback
fingerprinting is cold-start recovery for sessions already in flight, not the
primary identity model.

## Indexing

`mnemo` and `coding_agent_session_search` prove broad indexing is useful, but it
is not v1 for operations. Indexes add synchronization and freshness burden. For
move/resume correctness, Babel reads native storage directly and reports counts
or candidate paths in doctor mode.

## OpenAI Codex Source Check

Upstream `openai/codex` was checked at `6784db5` on 2026-05-01. The current
native resume surface is split:

- rollout JSONL under `~/.codex/sessions` carries transcript items, turn
  context, collaboration mode, plan deltas, and path-bearing metadata
- `~/.codex/state_*.sqlite` carries the thread index used for list/resume,
  including `threads.cwd`
- `thread_goals` is keyed by `thread_id`; it does not need a project-path rewrite
  for a directory move

So Codex move support must preserve the rollout files and rewrite both
`session_meta.payload.cwd` and `state_*.sqlite:threads.cwd`.

## Current Adapter Readiness

- Claude Code: doctor-only planner now; future apply must follow copy/verify,
  rewrite, rollback, and live-process refusal.
- Codex CLI: rollout JSONL plus `state_*.sqlite:threads.cwd`; generic apply has
  fixtures for JSONL, TOML, text refs, and SQLite text-column rewrites.
- Gemini CLI: project-hash storage is known; apply needs fixtures.
- Qwen Code: compatible hook identity; path storage needs fixtures.
- Cursor Agent: state roots are known, but SQLite/workspaceStorage migration
  needs close-app, backup, image copy, and restart discipline.
- Cline/Roo/Kilo/Copilot VS Code surfaces: recon only until extension storage
  contracts are mapped.
- OpenCode/Amp: in-process plugin models; bridge contracts, not stdin hook
  drop-ins.
- Aider: mostly project-local history; moving the directory should preserve the
  important files.
