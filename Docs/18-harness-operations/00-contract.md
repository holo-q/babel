# Harness Operations Contract

## Scope

Harness operations cover move, resume, handoff, archive, and recovery workflows.
`--doctor` is the shared diagnostic mode for commands in this family. It is
stronger than dry-run: it gathers evidence, reports blockers, shows operation
candidates, and exits without mutation.

## Invariant: Native Storage Is Truth

Native harness storage remains authoritative. Babel may scan, normalize, and
report storage, but it must not require a persistent global full-text index for
move/resume correctness. A cache is acceptable only when it can be rebuilt from
native adapters.

## Invariant: Mutation Requires Recovery

The credible apply contract comes from `cc-port` and `ccmv`, not from the old
Claude-only Babel mover:

- discover state roots and live processes before writes
- resolve paths through existing ancestors so symlinks do not produce false keys
- refuse destination key collisions before any write
- copy, verify, rewrite, and promote atomically where possible
- preserve malformed or opaque data instead of rewriting blindly
- keep backups and rollback paths for every touched root
- make interrupted/manual moves idempotent by scanning current state first

## Invariant: Session Identity Is Provider-Owned

Babel should prefer native session ids from hooks, environment variables, wrapper
metadata, or provider transcript files. Scrollback fingerprinting is cold-start
recovery for sessions already in flight, not the primary identity model.

## Invariant: Report Facts, Not Generic Harness Commentary

`babel mv --doctor` reports path-move storage facts. Hook support, general
installation status, daemon health, and UX capability belong to other doctor
surfaces unless they directly block the move.

## Claude Storage Contract

Claude storage is broader than `history.jsonl` plus `projects/<key>`. A complete
adapter must cover history, transcripts, project memory, settings, MCP/trust
metadata, todos, usage-data session metadata, usage facets, plugin data, tasks,
and opaque file-history snapshots.

## Claude Encoding Risk

The references disagree on Claude project-key encoding details. `cc-port`
replaces `/`, `.`, and space. `ccmv` reports that every non-alphanumeric
character except `-` becomes `-`. Babel doctor probes both key shapes and reports
what actually exists instead of pretending one stale helper is truth.

