# Babel Workspace Split Plan

## Problem

Single crate = 22,386 lines = no parallel compilation within babel itself.
Every source touch triggers 2m32s rebuild.

## Proposed Structure

```
claude-babel/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── babel-types/              # ~500 lines - Core types, zero deps
│   │   └── src/lib.rs            # PaneAddr, WindowInfo, SessionInfo, etc.
│   │
│   ├── babel-kitty/              # ~1500 lines - Kitty terminal integration
│   │   └── src/lib.rs            # kitty.rs → kitty IPC, pane queries
│   │   depends: babel-types
│   │
│   ├── babel-storage/            # ~1700 lines - Persistence layer
│   │   └── src/lib.rs            # babel_storage.rs + wset.rs
│   │   depends: babel-types, rusqlite
│   │
│   ├── babel-events/             # ~1000 lines - Event system
│   │   └── src/lib.rs            # events.rs
│   │   depends: babel-types
│   │
│   ├── babel-fingerprint/        # ~900 lines - Session fingerprinting
│   │   └── src/lib.rs            # fingerprint.rs
│   │   depends: babel-types, scrollparse
│   │
│   ├── babel-utility/            # ~2000 lines - Claude discovery/storage
│   │   └── src/                  # utility/*.rs
│   │   depends: babel-types, babel-kitty
│   │
│   ├── babel-daemon/             # ~3000 lines - The daemon
│   │   └── src/lib.rs            # daemon.rs (the 2702 line beast)
│   │   depends: babel-*, wnck-rs, gtk
│   │
│   ├── babel-core/               # ~1200 lines - Unified API (BabelCore)
│   │   └── src/lib.rs            # core.rs
│   │   depends: babel-daemon, babel-utility
│   │
│   ├── babel-tui/                # ~900 lines - TUI (optional feature)
│   │   └── src/                  # tui/*.rs
│   │   depends: babel-core, ratatui, crossterm
│   │
│   └── babel-pager/              # ~1200 lines - Pager (optional feature)
│       └── src/                  # pager/*.rs
│       depends: babel-core
│
├── src/
│   ├── main.rs                   # CLI entry (~300 lines)
│   └── cli/                      # CLI commands (~3500 lines)
│
└── babel (final binary)
```

## Dependency Graph (Compilation Order)

```
                    ┌─────────────┐
                    │ babel-types │  (compiles first, fast)
                    └──────┬──────┘
           ┌───────────────┼───────────────┬────────────────┐
           ▼               ▼               ▼                ▼
    ┌────────────┐  ┌────────────┐  ┌────────────┐  ┌─────────────┐
    │babel-kitty │  │babel-events│  │babel-storage│ │babel-fingerprint│
    └──────┬─────┘  └──────┬─────┘  └─────┬──────┘  └──────┬──────┘
           │               │              │                │
           └───────────────┴──────┬───────┴────────────────┘
                                  ▼
                         ┌───────────────┐
                         │ babel-utility │
                         └───────┬───────┘
                                 ▼
                         ┌───────────────┐
                         │ babel-daemon  │  (the big one, but isolated)
                         └───────┬───────┘
                                 ▼
                         ┌───────────────┐
                         │  babel-core   │
                         └───────┬───────┘
                    ┌────────────┴────────────┐
                    ▼                         ▼
             ┌────────────┐            ┌─────────────┐
             │ babel-tui  │            │ babel-pager │
             └────────────┘            └─────────────┘
                    │                         │
                    └────────────┬────────────┘
                                 ▼
                          ┌───────────┐
                          │   babel   │  (final binary)
                          └───────────┘
```

## Expected Build Time Improvements

| Scenario | Before | After |
|----------|--------|-------|
| Touch main.rs | 2m32s | ~20s (only cli + link) |
| Touch daemon.rs | 2m32s | ~40s (daemon + core + link) |
| Touch kitty.rs | 2m32s | ~30s (kitty + utility + daemon + core + link) |
| Touch types only | 2m32s | ~15s (types + dependents cascade, but parallel) |
| Touch tui/ | 2m32s | ~15s (tui + link) |

## Migration Steps

1. **Create workspace Cargo.toml** at root
2. **Extract babel-types** - pure types, no logic, zero internal deps
3. **Extract babel-kitty** - move kitty.rs, depends only on babel-types
4. **Extract babel-storage** - move babel_storage.rs + wset.rs
5. **Extract babel-events** - move events.rs
6. **Extract babel-fingerprint** - move fingerprint.rs
7. **Extract babel-utility** - move utility/*.rs
8. **Extract babel-daemon** - move daemon.rs (the big refactor)
9. **Extract babel-core** - move core.rs
10. **Extract babel-tui** - move tui/*.rs
11. **Extract babel-pager** - move pager/*.rs
12. **Update main.rs** - import from workspace crates
13. **Test everything** - ensure daemon and CLI work

## Risk Assessment

- **Low risk**: Types, storage, events - clean boundaries
- **Medium risk**: Daemon extraction - lots of cross-module deps
- **Mitigation**: Do incrementally, test after each extraction

## Alternative: Feature Flags

If full split is too disruptive, could use feature flags to exclude TUI/pager
from default builds (saves ~2100 lines from compilation).

```toml
[features]
default = []
tui = ["dep:ratatui", "dep:crossterm"]
pager = []
```

This is simpler but gives less parallelism benefit.
