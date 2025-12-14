# kitty.rs Implementation Summary

## Overview

Implemented a complete Rust wrapper for kitty's remote control protocol in `/home/nuck/Workspace/Plugins/claude-babel/src/kitty.rs`.

## Architecture

The module follows kitty's JSON hierarchy:
```
OS Window -> Tabs -> Windows (panes)
```

And flattens it into a simple list of windows for easier consumption.

## Public API

### Data Structures

**`KittyWindow`**
- `id: u64` - Unique window identifier
- `title: String` - Window title
- `cwd: PathBuf` - Current working directory
- `is_focused: bool` - Whether this window has focus
- `is_active: bool` - Whether this window is active within its tab
- `foreground_processes: Vec<ForegroundProcess>` - Running processes
- `user_vars: HashMap<String, String>` - Custom metadata (for babel state)

**`ForegroundProcess`**
- `pid: u32` - Process ID
- `cmdline: Vec<String>` - Command line arguments
- `cwd: PathBuf` - Process working directory

### Functions

| Function | Description |
|----------|-------------|
| `list_windows() -> Result<Vec<KittyWindow>>` | List all kitty windows across all OS windows/tabs |
| `find_claude_windows() -> Result<Vec<KittyWindow>>` | Find windows running claude (filters by cmdline) |
| `get_window(id: u64) -> Result<Option<KittyWindow>>` | Get specific window by ID (None if closed) |
| `focus_window(id: u64) -> Result<()>` | Focus a window (may need focus-steal wrapper) |
| `set_user_var(id: u64, key: &str, value: &str) -> Result<()>` | Set persistent metadata on window |
| `get_scrollback(id: u64) -> Result<String>` | Get entire scrollback buffer |
| `send_text(id: u64, text: &str) -> Result<()>` | Send text to window input (no newline) |
| `get_focused_cwd() -> Result<Option<PathBuf>>` | Get CWD of focused window |

## Implementation Details

### JSON Parsing

Uses internal `Raw*` structs to deserialize kitty's JSON format:
- `RawOsWindow` -> `RawTab` -> `RawWindow` -> `RawForegroundProcess`

Then transforms into public API structs, flattening the hierarchy.

### Command Execution

All functions execute `kitten @` commands via `std::process::Command`:
```rust
Command::new("kitten")
    .args(["@", "ls"])
    .output()
```

Errors are properly contextualized using `anyhow::Context`.

### Socket Path

Kitty automatically uses `$XDG_RUNTIME_DIR/kitty.sock` or `/tmp/mykitty-*`.
No need to specify socket path explicitly (kitty remote control handles it).

## Integration Points

### claude-fire
- `get_focused_cwd()` - Determine context for new prompts

### overlay
- `find_claude_windows()` - Discover active sessions
- `list_windows()` - Full window inventory

### state
- `set_user_var()` - Store conversation_id, last_updated
- `user_vars` field - Retrieve stored metadata

### discovery
- All functions used for session correlation

## Testing

Integration tests included (marked `#[ignore]`, require running kitty):
```bash
cargo test -- --ignored
```

Tests verify:
- Window listing works
- Claude process detection works
- Focused window CWD retrieval works

## Compilation Status

✅ Module compiles successfully
✅ No errors in kitty.rs
✅ Library builds (warnings are in other modules)

## Example Usage

```rust
use claude_babel::kitty;

// Find all claude sessions
let sessions = kitty::find_claude_windows()?;
for win in sessions {
    println!("Claude in window {}: {}", win.id, win.title);
    
    // Store metadata
    kitty::set_user_var(win.id, "conversation_id", "abc123")?;
    
    // Send a prompt
    kitty::send_text(win.id, "Explain this code\n")?;
}

// Get context for new session
if let Some(cwd) = kitty::get_focused_cwd()? {
    println!("Starting claude in: {}", cwd.display());
}
```

## Notes

- Focus stealing may require `focus-steal` wrapper (see Bin/focus-steal)
- Scrollback can be large - consider adding limits in the future
- User vars persist across kitty restarts (stored in kitty's session)
- All functions are module-level (no KittyControl struct - simpler API)
