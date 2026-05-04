# Conversation Pager Specification

A reusable TUI tool for viewing, navigating, and selecting messages from Claude Code conversations. Inspired by Codex CLI's transcript viewer but designed to be better and more general-purpose.

## Vision

```
scrollparse-pager - Universal conversation pager for Claude Code sessions

USAGE:
    scrollparse-pager [OPTIONS] [FILE]
    babel scrollback <id> | scrollparse-pager
    cat session.jsonl | scrollparse-pager --format=jsonl

OPTIONS:
    -f, --format <FMT>     Input format: auto, scrollback, jsonl (default: auto)
    -s, --select           Enable selection mode (output selected to stdout)
    -o, --output <FMT>     Output format: text, json, indices (default: text)
    --no-preview           Disable preview pane
    --follow               Follow mode for streaming input (like tail -f)
```

## Core Features

### 1. Message Navigation

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ Conversation Pager                           [12 messages] [session: abc123]│
├─────────────────────────────────┬───────────────────────────────────────────┤
│  1  > User                      │ How do I list files in the current       │
│     How do I list files...      │ directory?                               │
│                                 │                                          │
│  2  ● Claude                    │                                          │
│     You can use the ls...       │                                          │
│                                 │                                          │
│▸ 3  ● Bash(ls -la)              │ $ ls -la                                 │
│     ⎿ total 42...               │ total 42                                 │
│                                 │ drwxr-xr-x 2 user user 4096 Dec 9 .      │
│  4  ● Claude                    │ -rw-r--r-- 1 user user 1234 file.txt     │
│     The output shows...         │                                          │
│                                 │                                          │
│  5  > User                      │                                          │
│     Thanks! Can you also...     │                                          │
├─────────────────────────────────┴───────────────────────────────────────────┤
│ j/k:nav  v:visual  space:toggle  /:search  enter:output  q:quit  ?:help    │
└─────────────────────────────────────────────────────────────────────────────┘
```

**Navigation keys** (vim-style):
| Key | Action |
|-----|--------|
| `j` / `↓` | Next message |
| `k` / `↑` | Previous message |
| `J` / `K` | Next/prev by 5 |
| `g` / `Home` | First message |
| `G` / `End` | Last message |
| `Ctrl-D` | Half page down |
| `Ctrl-U` | Half page up |
| `PgDn` / `Space` | Page down |
| `PgUp` / `Shift-Space` | Page up |

### 2. Selection Mode (`-s` / `--select`)

**Single selection:**
- `Enter` → output current message and exit
- `Space` → toggle selection marker on current message

**Multi-selection:**
- `Space` → toggle selection on current message
- `a` → select all
- `A` → deselect all
- `Enter` → output all selected messages and exit

**Visual mode (range selection):**
- `v` → enter visual mode at cursor
- Move cursor → extends selection range
- `V` → visual line mode (select full messages)
- `Enter` → confirm and output range
- `Esc` → cancel visual mode

### 3. Search & Filter

- `/` → enter search mode
- Type query → filter messages containing text
- `n` → next match
- `N` → previous match
- `Esc` → clear search, show all
- `t` → filter by type (user/assistant/tool/output)

### 4. Preview Pane

Right pane shows full content of current message:
- Syntax highlighting for code blocks
- Proper diff rendering for Edit tool output
- Wrap long lines intelligently
- Scroll independently with `h`/`l` or arrow keys when focused

### 5. Output Formats

When selection is confirmed (`Enter`):

**text** (default):
```
> How do I list files?

● You can use the ls command...

● Bash(ls -la)
  ⎿ total 42
     drwxr-xr-x ...
```

**json**:
```json
[
  {"index": 0, "kind": "user", "content": "How do I list files?"},
  {"index": 1, "kind": "assistant", "content": "You can use..."},
  {"index": 2, "kind": "tool_call", "name": "Bash", "args": "ls -la"},
  {"index": 3, "kind": "tool_output", "content": "total 42\n..."}
]
```

**indices** (for piping):
```
0
1
2
3
```

## Architecture

### Crate Structure

```
scrollparse/
├── src/
│   ├── lib.rs           # Existing library
│   ├── claude.rs        # Existing Claude parser
│   ├── bin/
│   │   └── scrollparse-pager.rs  # NEW: TUI binary
│   ├── pager/           # NEW: Pager module
│   │   ├── mod.rs
│   │   ├── app.rs       # App state, event loop
│   │   ├── ui.rs        # Rendering
│   │   ├── selection.rs # Selection state machine
│   │   ├── search.rs    # Search/filter logic
│   │   └── input.rs     # Input sources (stdin, file, kitty)
│   └── ...
└── Cargo.toml           # Add ratatui, crossterm deps
```

### Key Components

#### 1. `PagerApp` - Main State

```rust
pub struct PagerApp {
    /// Parsed messages
    messages: Vec<Message>,
    /// Session metadata
    session: ParsedSession,

    /// Current cursor position
    cursor: usize,
    /// Scroll offset in message list
    scroll_offset: usize,
    /// Scroll offset in preview pane
    preview_scroll: usize,

    /// Selection state
    selection: SelectionState,
    /// Search state
    search: SearchState,

    /// Which pane is focused
    focus: PaneFocus,
    /// Output format when exiting
    output_format: OutputFormat,
    /// Whether we're in selection mode
    selection_mode: bool,
}
```

#### 2. `SelectionState` - Multi-select + Visual Mode

```rust
pub enum SelectionState {
    /// No selection active
    None,
    /// Individual items toggled
    Toggled(HashSet<usize>),
    /// Visual mode range
    Visual {
        anchor: usize,
        cursor: usize,
    },
}

impl SelectionState {
    fn selected_indices(&self, cursor: usize) -> Vec<usize>;
    fn is_selected(&self, index: usize, cursor: usize) -> bool;
    fn toggle(&mut self, index: usize);
    fn enter_visual(&mut self, cursor: usize);
    fn extend_visual(&mut self, cursor: usize);
    fn confirm_visual(&mut self) -> Vec<usize>;
}
```

#### 3. `InputSource` - Multiple Input Types

```rust
pub enum InputSource {
    /// Read from file path
    File(PathBuf),
    /// Read from stdin (already consumed)
    Stdin(String),
    /// Stream from kitty scrollback (for --follow)
    KittyScrollback { window_id: u64 },
}

pub enum InputFormat {
    /// Auto-detect from content
    Auto,
    /// Terminal scrollback (Claude Code output)
    Scrollback,
    /// Claude's JSONL session files
    Jsonl,
}
```

### Dependencies

```toml
[dependencies]
# Existing
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# New for pager
ratatui = "0.28"
crossterm = "0.28"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time"] }
```

## Implementation Plan

### Phase 1: Core Pager (MVP)

1. **Basic navigation** - j/k/g/G/PgUp/PgDn
2. **Message list rendering** - Left pane with truncated previews
3. **Preview pane** - Full message content
4. **Single selection** - Enter to output and exit
5. **Stdin input** - Pipe scrollback in

**Deliverable**: `cat scrollback.txt | scrollparse-pager` works

### Phase 2: Selection Features

1. **Multi-select** - Space to toggle, Enter to output all
2. **Visual mode** - v for range, V for line mode
3. **Select all/none** - a/A shortcuts
4. **Output formats** - text/json/indices

**Deliverable**: Full selection workflow for extracting message ranges

### Phase 3: Search & Polish

1. **Search mode** - / to search, n/N to navigate
2. **Type filter** - t to filter by message type
3. **Syntax highlighting** - Code blocks, diffs
4. **Scroll indicators** - Progress bar, message count

### Phase 4: Advanced Features

1. **JSONL input** - Parse Claude session files directly
2. **Follow mode** - --follow for streaming
3. **Kitty integration** - Read scrollback via kitty protocol
4. **Theme support** - Respect terminal colors

## Codex Patterns to Steal

### From `pager_overlay.rs`:

1. **Height caching** - `CachedRenderable` wraps items with cached heights, recalculates on width change
2. **Scroll continuity** - Preserves logical scroll position across resizes
3. **Follow-bottom** - Auto-scroll to new content in streaming mode
4. **Scrollbar rendering** - Footer shows position indicator

### From `resume_picker.rs`:

1. **Search-as-you-type** - Filter while typing, don't wait for Enter
2. **Pagination tokens** - For lazy-loading large conversations
3. **Deduplication** - HashSet to track seen items
4. **Column metrics** - Dynamic width calculation with eliding

### From `selection_list.rs`:

1. **Selection indicator** - `›` prefix for selected, space for unselected
2. **Index display** - 1-indexed numbers for quick reference
3. **Color styling** - Cyan for selected items

## Integration with Babel

### CLI Command

```bash
# View scrollback of a Claude pane
babel pager <window_id>

# View with selection mode
babel pager <window_id> --select

# Pipe selection to clipboard
babel pager <window_id> --select | xclip -selection clipboard
```

### Implementation in `cli/action.rs`:

```rust
pub async fn cmd_pager(core: &BabelCore, window_id: u64, select: bool) -> Result<()> {
    let scrollback = core.scrollback(window_id, None).await?;

    // Launch pager with scrollback
    scrollparse_pager::run(
        InputSource::Stdin(scrollback),
        PagerOptions {
            selection_mode: select,
            ..Default::default()
        }
    )?;

    Ok(())
}
```

## Testing Strategy

1. **Snapshot tests** - Render output at various terminal sizes
2. **Selection state machine** - Unit tests for visual mode transitions
3. **Search filtering** - Property tests for filter correctness
4. **Input parsing** - Test both scrollback and JSONL formats

## Open Questions

1. **Location**: Should this live in scrollparse crate or separate `scrollparse-pager` crate?
   - Recommendation: In scrollparse as `bin/scrollparse-pager.rs` + `pager/` module

2. **JSONL parsing**: Should we add JSONL parsing to scrollparse or keep it in babel?
   - Recommendation: Add to scrollparse for completeness

3. **Kitty integration**: Direct protocol vs shelling out to `kitten @`?
   - Recommendation: Shell out for simplicity, same as babel

## References

- [OpenAI Codex CLI](https://github.com/openai/codex) - TUI patterns, MIT license
- [ratatui](https://github.com/ratatui-org/ratatui) - TUI framework
- scrollparse existing code at `/home/nuck/Workspace/Tools/scrollparse/`
- babel TUI at `/home/nuck/Workspace/Daemons/claude-babel/src/tui/`
