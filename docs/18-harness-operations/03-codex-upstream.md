# Codex Upstream Verification

## Reference: `openai/codex`

Clone path: `references/openai-codex`.

Checked commit: `6784db5` from 2026-05-01.

Purpose: verify Codex move semantics against first-party source instead of only
third-party session browsers.

## Harness: Codex Native Resume Surface

Current Codex native resume is split:

- rollout JSONL under `~/.codex/sessions` carries transcript items, turn
  context, collaboration mode, plan deltas, and path-bearing metadata
- `state_*.sqlite` carries the thread index used for list/resume, including
  `threads.cwd`; its root is `config.toml` `sqlite_home` when set, then
  `CODEX_SQLITE_HOME`, otherwise `~/.codex`
- `thread_goals` is keyed by `thread_id`; it does not need a project-path rewrite
  for a directory move

## Source Evidence

Relevant upstream files:

- `codex-rs/rollout/src/recorder.rs`
- `codex-rs/rollout/src/state_db.rs`
- `codex-rs/rollout/src/session_index.rs`
- `codex-rs/rollout/src/metadata.rs`
- `codex-rs/config/src/config_toml.rs`
- `codex-rs/state/src/lib.rs`
- `codex-rs/state/migrations/0001_threads.sql`
- `codex-rs/state/migrations/0027_threads_cwd_sort_indexes.sql`
- `codex-rs/state/migrations/0029_thread_goals.sql`
- `codex-rs/state/src/runtime/threads.rs`
- `codex-rs/core/src/session/turn_context.rs`
- `codex-rs/protocol/src/protocol.rs`

## Operation: Codex Move

Codex move support must preserve rollout files and rewrite both
`session_meta.payload.cwd` and `state_*.sqlite:threads.cwd`. The SQLite scan
must follow the configured SQLite root, not just `~/.codex`, because first-party
source allows the thread index to be split from the transcript/config home.

`session_index.jsonl` is append-only id/name metadata and does not carry cwd.
It is not a project move rewrite target.

## Operation: Codex Plan Mode

Codex `/plan` mode is carried by rollout records. `TurnContextItem` serializes
`collaboration_mode`, and plan streaming uses `PlanDeltaEvent`. Moving a project
must not invent a separate plan-state migration surface; preserving and rewriting
the rollout JSONL is the correct layer.

`thread_goals` is a separate SQLite table keyed by `thread_id`. It may matter
for goal UX, but it is not path-keyed. Because thread ids remain unchanged, the
table moves with the Codex state DB without a cwd rewrite.
