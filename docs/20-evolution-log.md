# Babel Evolution Log

This file is the compressed trajectory of the recent Babel jump. It exists so
future agents do not restart from older "Claude Babel" assumptions after the
repo move and refactor waves.

## 2026-05-07: Babel, Not Claude Babel

Babel is now the canonical project name and path. Treat references to
`claude-babel` as historical unless they describe Claude Code's native storage
or protocol specifically.

Current source lives at:

```text
/home/nuck/holoq/repo-os/babel
```

The architectural center also shifted:

- **Old shape:** Claude-first kitty helper with useful side commands.
- **Current shape:** harness-aware agent session substrate with terminal
  adapters, native storage migration, resume/history ergonomics, paint streams,
  and durable overlay state.

## Migration Became Real Infrastructure

`babel mv` graduated from an old Claude-only idea into a generic mutation
planner/executor:

- `babel mv --doctor OLD NEW` is evidence mode: it reports live panes, native
  harness storage roots, typed planned edits, risks, and preserve-only surfaces
  without mutation.
- `babel mv OLD NEW` applies typed mutation atoms, emits each mutation as it
  happens, snapshots session-owned state, verifies mutated targets, and rolls
  back owned state on failure.
- Native storage remains source of truth. Indexing is allowed only as a
  rebuildable acceleration layer, never as the canonical migration source.
- Claude Code and Codex are the daily-driver mutation paths. The rest of the
  roster is intentionally explicit about apply readiness, preservation, and
  unsupported surfaces.

This was validated by moving many live projects, including Babel itself. Do not
replace this with cwd/time/title guessing or a hidden global index.

## Harness Roster And Support Bar

The hook/support matrix is no longer vague. Babel tracks harnesses through:

- stable native session identity;
- lifecycle hook vocabulary or bridge callbacks;
- native transcript/session storage;
- migration capability, separate from hook capability;
- maintenance level, separate from theoretical adapter support.

Claude Code and Codex CLI are the guaranteed mainline paths. Other harnesses are
rostered, documented, and probed where useful, but they need real users, traces,
and PRs before they can be treated as guaranteed.

Prior art under ignored `references/` has been consumed as research material,
not vendored code. Durable conclusions belong in `docs/18-harness-operations/`,
tests, adapter comments, or README matrices.

## Refactor Waves: What Is Now True

The refactor direction is module-first, not workspace-first. Workspace crate
splitting remains optional and must follow proven module boundaries.

The major internal truths now in place:

- `model::PaneAddr` is the canonical pane identity; raw kitty ids are legacy
  edge inputs.
- Activity state has a reducer path and explicit precedence between hook truth
  and scrollback evidence.
- Refresh and matching are service concepts, not daemon side effects.
- Matching resolves batch candidates through one coordinator before claiming
  sessions, avoiding parallel double-claims.
- Events and IPC are moving toward stable DTOs outside daemon internals.
- Backend discovery is registry-shaped, with kitty as the reference backend and
  tmux/zellij represented through explicit adapter capability rather than
  magical terminal assumptions.

## Resume Became A Product Surface

`babel resume` is no longer a thin picker. It is now the session command center:

- native sessions carry created/modified times;
- rows render created time, modified time, turn/token tone, cwd mode, title, and
  prompt columns;
- history can be addressed by UUID/native id;
- transcript preview has role filters, condensed mode, ANSI/control stripping,
  and stable native-id title binding;
- identity copy emits disk-shaped JSON for debugging native storage provenance;
- display preferences persist outside session metadata;
- touched-project metrics and workgroup coloring make multi-project sessions
  visible without introducing a fragile global transcript index.

CLI surfaces evolved with it:

- `babel ls --history` and `babel ls --history-recursive` bridge live panes and
  durable history.
- `babel ls-sessions --uuid` exposes stable ids for follow-up commands.
- `babel prompts` gives directory-scoped prompt history with optional context.
- `babel cat <uuid>` gives a pipe-friendly collapsed transcript for a native id.

Current frontier work is tightening project-focus filtering, prompt filtering,
and denser token/turn display. Keep these as resume UX concerns, not daemon
state.

## Paint And Panels

Babel's panel contract is the typed paint stream. Consumers such as richmon and
richspace should not reclassify sessions or invent colors. They receive resolved
window/workspace paint payloads and render them.

Unread/ring work belongs in Babel's activity/read model and paint output first,
then panel renderers. If a ring is too subtle, thicken the renderer; do not fork
session truth into panel-local state.

## Repo-OS Context

`vtr` and `scrollparse` have been promoted into `repo-os`. This is expected.
Babel should treat them as local platform libraries, not alarming external
drift.

## Non-Regression Rules

- Do not reintroduce personal absolute paths in production code or new tests.
- Do not make `harness_ops` absorb every harness detail forever; split by
  operation capability and shared mutation atoms when the next compression wave
  arrives.
- Do not make `--doctor` mean one global diagnostic. `babel --doctor` may report
  installation/daemon/hook health; `babel mv --doctor` reports migration facts.
- Do not turn migration verification into a scan-only promise. Verification is
  post-mutation proof over the exact owned targets that were changed.
- Do not use a global FTS/index as the source of truth for move/resume. It may
  be useful later, but it must remain rebuildable.
