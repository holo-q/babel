# Harness Operations Board

This is the entry point for Babel's harness-operation research and migration
contract. The detailed pages live in `Docs/18-harness-operations/` so ripmap,
grep, and future report indexing can land on a narrow document instead of one
long mixed board.

## Indexing Contract

Each harness-operation page should use stable nouns in headings:

- `Harness:` for provider-specific storage and migration facts
- `Operation:` for typed actions emitted by doctor mode
- `Report:` for CLI output shape and diagnostics
- `Reference:` for consumed external repos under `references/`
- `Invariant:` for rules that must remain true during mutation

Do not store a global search index as source of truth. Native harness storage is
authoritative; any generated index must be rebuildable.

## Pages

- [Contract](18-harness-operations/00-contract.md)
- [Report Shape](18-harness-operations/01-report-shape.md)
- [Adapter Matrix](18-harness-operations/02-adapter-matrix.md)
- [Codex Upstream Verification](18-harness-operations/03-codex-upstream.md)
- [Reference Ledger](18-harness-operations/04-reference-ledger.md)
