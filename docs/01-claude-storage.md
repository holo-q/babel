# Claude Storage Implementation

## Summary

Successfully implemented `claude_storage.rs` module for parsing Claude Code's JSONL-based conversation storage at `~/.claude/`.

## Architecture

### Storage Structure
```
~/.claude/
├── history.jsonl               # Global history with display titles
└── projects/
    └── {-path-encoded}/        # e.g., -home-nuck-Workspace
        └── {session-id}.jsonl  # Full conversation files
```

### JSONL Format
Each session file contains:
- **Summaries** (top of file): `{"type": "summary", "summary": "...", "leafUuid": "..."}`
- **Messages**: `{"type": "user"|"assistant", "sessionId": "...", "cwd": "/path", ...}`
- **Metadata**: file-history-snapshot, etc.

## Implementation Details

### Core Functions

1. **`claude_base()`** - Returns `~/.claude` path
2. **`list_projects()`** - Lists all project directories
3. **`list_sessions(project)`** - Lists session files in a project
4. **`get_session_summaries(path)`** - Fast summary extraction (first 20 lines)
5. **`get_session_info(path)`** - Extracts metadata without full parse
6. **`find_session_by_summary(query)`** - Fuzzy search across all sessions
7. **`get_recent_sessions(limit)`** - Recent sessions from history.jsonl

### Key Design Decisions

1. **Streaming JSONL parsing** - Never loads entire files into memory
2. **Early termination** - Stops reading after finding needed data (summaries in first ~30 lines)
3. **Path encoding** - `/home/user/project` → `-home-user-project` directory name
4. **Window title stripping** - Removes `✳ ` prefix before fuzzy matching
5. **Deduplication** - history.jsonl may have duplicates; dedupe by session_id

### Data Structures

```rust
pub struct Summary {
    pub summary: String,
    pub leaf_uuid: Option<String>,
}

pub struct SessionInfo {
    pub session_id: String,
    pub project: PathBuf,
    pub summaries: Vec<Summary>,
    pub slug: Option<String>,
    pub cwd: Option<PathBuf>,
    pub last_timestamp: Option<String>,
}
```

## Testing

All unit tests pass:
- `test_strip_title_prefix` - Unicode prefix removal
- `test_fuzzy_match` - Case-insensitive substring matching
- `test_path_to_project_dir` - Path encoding

Integration test (`examples/test_storage.rs`) successfully:
- Lists 22 projects
- Retrieves 5 recent sessions with summaries
- Fuzzy-finds session by partial summary text

## Performance

- **Summary extraction**: O(1) - reads only first ~20 lines
- **Session info**: O(n) where n ≈ 30-50 lines (early termination)
- **Recent sessions**: O(m) where m = history.jsonl size (deduplicated)
- **Summary search**: O(p×s) where p=projects, s=sessions (but fast due to early termination)

## Integration with babel

This module provides the foundation for:
1. **Session discovery** - Match kitty windows to Claude sessions
2. **Overlay UI** - Display session summaries in rofi/dmenu
3. **Navigation** - Jump to sessions by fuzzy-matching titles
4. **State tracking** - Monitor active sessions across windows

## Files Modified

- `~/Workspace/Plugins/babel/src/claude_storage.rs` - Complete rewrite
- `~/Workspace/Plugins/babel/src/discovery.rs` - Updated to use `SessionInfo` instead of old SQLite types
- `~/Workspace/Plugins/babel/examples/test_storage.rs` - Integration test

## Future Enhancements

1. **Caching** - Cache parsed session info to avoid re-reading files
2. **Watchers** - Detect new sessions via inotify/FSEvents
3. **Full message parsing** - When needed for advanced features
4. **Branch tracking** - Follow conversation branches via leafUuid
5. **Search index** - Build inverted index for faster fuzzy search
