# Codex IO Surface

## Reference: `openai/codex` Write Audit

Clone path: `references/openai-codex`.

Checked commit: `6784db5` from 2026-05-01.

Audit command shape:

```sh
rg '\b(fs::write|std::fs::write|tokio::fs::write|write_atomically|OpenOptions|File::create|create_new|create_dir_all|remove_file|remove_dir_all|rename\(|copy\(|write_all|persist\(|NamedTempFile|set_len\()' \
  references/openai-codex/codex-rs --glob '*.rs' --glob '!**/*_tests.rs' --glob '!**/tests/**' --glob '!**/vendor/**'
```

Narrow follow-up:

```sh
rg '\b(write_atomically|OpenOptions|create_new|append\(true\)|File::create|create_dir_all|remove_file|remove_dir_all|rename\(|persist\(|fs::write|std::fs::write|tokio::fs::write)' \
  references/openai-codex/codex-rs/{rollout,state,config,core,core-plugins,core-skills,login,tui,cli,arg0,utils}/ \
  --glob '*.rs' --glob '!**/*_tests.rs' --glob '!**/tests/**'
```

## Harness: Migration-Relevant Writes

- `rollout/src/recorder.rs`: appends rollout JSONL under `sessions/YYYY/MM/DD`
  and reconciles rollout items into the state DB.
- `rollout/src/session_index.rs`: appends `session_index.jsonl`; entries are
  id/name/update metadata and are not cwd identity.
- `state/src/runtime.rs`: opens `state_<version>.sqlite` and
  `logs_<version>.sqlite` under the configured SQLite root.
- `state/src/runtime/threads.rs`: persists `threads.cwd`, `threads.rollout_path`,
  and related thread index metadata.
- `core/src/message_history.rs`: appends `history.jsonl`.
- `core/src/shell_snapshot.rs`: writes, renames, and removes shell snapshots.
- `core/src/config/edit.rs` and `config/src/*_edit.rs`: atomically rewrite
  `config.toml`.

## Invariant: Codex Project Move

The move identity surfaces are:

- rollout `session_meta.payload.cwd`
- `config.toml` `[projects."<cwd>"]`
- state DB `threads.cwd`
- text references in rollout/history/config sidecars

The preservation-only surfaces are:

- shell snapshots keyed by session id
- `thread_goals`, dynamic tools, spawn edges, and memories keyed by thread id
- `session_index.jsonl` id/name rows

Auth, plugin caches, skill caches, update markers, helper shims, and TUI logs
are Codex-owned writes, but not project-cwd identity for `babel mv`.

