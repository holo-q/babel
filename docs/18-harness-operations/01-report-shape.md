# Harness Operation Report Shape

## Report: `babel mv --doctor`

The move doctor report should be a structural inventory, not an entry dump. It
should summarize breadth and depth first, then show representative paths or
operations only where they change the decision.

Current top-level shape captured from the working report:

```text
babel mv --doctor - Harness Migration Report
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  summary <ops> op(s)  <blockers> blocker(s)  <warnings> warning(s)
  source  <absolute old path>
  dest    <absolute new path>
  policy  native storage is source of truth; indexing is rebuildable
  mode    doctor only - no mutation

Live Panes:
  <status> <harness> pane:<id> <activity> <cwd>

Harnesses:
  <harness> sessions:<n> refs:<n> ops:<n>
    root <state root>
    op <operation> plan <target> (<detail>)
    note <only move-relevant evidence>

Risks:
  <severity> <scope> <message>
```

## Report: Current Useful Signals

The current report shape is useful because it separates:

- source/destination normalization
- live panes that may still depend on the source path
- per-harness state roots
- session counts, path-reference counts, and operation counts
- concrete typed operations
- global risks

Keep those signals. Optimize noisy harnesses by collapsing repeated database
rows or workspace entries into structural summaries.

## Report: Disregarded Harnesses

Harnesses with no detected state should remain visible but de-emphasized. Their
paths should be dimmed, and their notes should avoid irrelevant capability
commentary.

Use this distinction:

- `not-detected`: no relevant root exists on this machine
- `no-matching-state`: root exists, but no state matches the source path
- `doctor-only`: root or references exist, but apply is not safe yet
- `apply-ready`: typed edits can be executed by the shared executor

## Report: Aggregation Rules

SQLite databases, workspace stores, and project-wide transcript roots can become
huge. The doctor report should aggregate:

- counts by storage kind
- counts by path-bearing record family
- apply-ready operation kinds
- sample paths only when needed to prove the shape
- omitted counts for suppressed entries

Do not print one `op` per workspace database row when a single structural
operation and count communicates the same decision.

## Operation: Typed Migration Edit

Doctor emits typed migration edits. Apply consumes those edits generically.
Harness modules should describe native storage; they should not each implement
their own mutation engine.

Current generic edit classes:

- `RenamePath`
- `RewriteJsonlField`
- `RewriteTomlTableKey`
- `RewriteTextRefs`
- `RewriteSqliteTextColumn`
- `PreserveSessionKeyedFiles`
- `PreserveProjectLocalHistory`

## Operation: Verification

Verification is post-mutation, not a replacement for the scan. The scan proves
what Babel intends to touch. Verification proves the resulting state actually
matches the intent after filesystem, SQLite, JSONL, TOML, and process-lock
corner cases have had a chance to interfere.
