# Harness Reference Ledger

## Reference: Storage Browser And Handoff Projects

These projects are useful for parser knowledge and session discovery, but most
are not lossless native movers:

- `cli-continues`: cross-agent handoff/resume knowledge across many harnesses
- CASR: canonical IR and provider-native session writing experiments
- Codbash: dashboard/search/resume/conversion knowledge
- `mnemo`: local native-storage indexing across many harnesses
- `coding_agent_session_search`: broad connector tests and storage fixtures
- CCManager: session/worktree management patterns
- Chronicle / `claude-history-manager`: browser/parser knowledge for Claude,
  Codex, and Gemini

## Reference: Claude Move Projects

These are the strongest references for native move safety:

- `cc-port`
- `ccmv`
- `claudepath`

Consumed ideas:

- backup before mutation
- live-session checks
- project-key rewrites
- transcript preservation
- history/settings/plugin/task/todo coverage
- rollback/restore workflows

## Reference: Indexing Projects

`mnemo` and `coding_agent_session_search` prove broad indexing is useful. They
also show why Babel should not make indexing the source of truth for v1 moves:
freshness, synchronization, and repair semantics become a second storage system.

Babel's operation layer should instead scan native roots directly and emit a
rebuildable report.

## Reference: Deprecated And Consumed

The references under `references/` are local research material. Babel consumes
their proven storage facts into this docs set and into adapter tests. If a
reference claim is not backed by first-party source, local fixture, or observed
state, keep the adapter `doctor-only`.

