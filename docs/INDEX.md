# babel Documentation Index

## Research Reports

### Kitty Cross-Instance Window Spawning

**Research Question:** Can kitty spawn windows in specific instances (e.g., spawn in instance 1234 from instance 5678)?

**Answer:** ✅ Yes, fully supported via `--to` flag.

**Files:**

1. **`RESEARCH-FINDINGS.md`** (root directory)
   - Executive summary
   - Key findings
   - Integration strategy
   - Next steps

2. **`docs/kitty-cross-instance-spawning.md`**
   - Comprehensive research report
   - Mechanism deep-dive
   - Use cases and examples
   - Edge cases and gotchas
   - Integration patterns

3. **`docs/kitty-routing-quick-reference.md`**
   - Quick reference guide
   - Common operations
   - Code snippets for integration
   - Debugging tips

4. **`docs/kitty-routing-architecture.txt`**
   - Visual architecture diagrams
   - Resolution hierarchy
   - Routing modes
   - Implementation checklist

## Tools and Scripts

### Testing

**`tools/test-cross-instance-spawn.sh`**
- Automated test suite
- Validates cross-instance spawning
- Verifies window tracking and remote control
- Run: `./tools/test-cross-instance-spawn.sh`

### Management

**`tools/kitty-instances.sh`**
- Discovery and management utility
- Commands:
  - `list` - List all running kitty instances
  - `sockets` - List socket paths only
  - `pids` - List PIDs only
  - `current` - Show current instance
  - `info <PID>` - Show detailed info for instance
  - `spawn <PID>` - Spawn test window in instance
  - `clean` - Remove stale sockets
- Run: `./tools/kitty-instances.sh --help`

## Quick Start

### List all kitty instances

```bash
/home/nuck/Workspace/Daemons/babel/tools/kitty-instances.sh list
```

### Spawn window in specific instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-{PID} launch --title "Test" bash
```

### Test cross-instance spawning

```bash
/home/nuck/Workspace/Daemons/babel/tools/test-cross-instance-spawn.sh
```

### Read the quick reference

```bash
cat /home/nuck/Workspace/Daemons/babel/docs/kitty-routing-quick-reference.md
```

## Integration Roadmap

See **`RESEARCH-FINDINGS.md`** section "Next Steps for babel" for detailed integration phases.

### Phase 1: Socket Storage
- Store daemon's `KITTY_LISTEN_ON` on startup
- Write to `/run/user/1000/babel-kitty.sock`

### Phase 2: Routing Module
- Create `src/utility/kitty.rs`
- Implement smart routing with fallback
- Route CLI output to daemon's kitty

### Phase 3: Mode Indicator
- Update TUI to show routing status
- Display "DAEMON (remote)", "DAEMON (local)", or "LOCAL"

## File Tree

```
/home/nuck/Workspace/Daemons/babel/
├── RESEARCH-FINDINGS.md          # Executive summary
├── docs/
│   ├── INDEX.md                  # This file
│   ├── kitty-cross-instance-spawning.md    # Full research report
│   ├── kitty-routing-quick-reference.md    # Quick reference
│   └── kitty-routing-architecture.txt      # Architecture diagrams
└── tools/
    ├── test-cross-instance-spawn.sh        # Test suite
    └── kitty-instances.sh                  # Management utility
```

## Key Concepts

### Socket Path Format

```
/run/user/{UID}/kitty.sock-{PID}
```

Example: `/run/user/1000/kitty.sock-1234`

### Resolution Hierarchy

1. `--to` flag (highest priority)
2. `KITTY_LISTEN_ON` environment variable
3. Controlling terminal (lowest priority)

### Remote Control Requirement

All instances must have:

```conf
# kitty.conf
allow_remote_control yes
```

Or started with: `kitty -o allow_remote_control=yes`

## Common Operations

### Spawn window in daemon's instance

```bash
daemon_sock=$(cat /run/user/1000/babel-kitty.sock)
kitty @ --to "$daemon_sock" launch --title "Output" tail -f log.txt
```

### Check if daemon socket is alive

```bash
daemon_sock=$(cat /run/user/1000/babel-kitty.sock)
if kitty @ --to "$daemon_sock" ls &>/dev/null; then
    echo "Daemon kitty is alive"
fi
```

### Fallback to local instance

```bash
if [[ -f /run/user/1000/babel-kitty.sock ]]; then
    sock=$(cat /run/user/1000/babel-kitty.sock)
    kitty @ --to "$sock" launch ...
else
    kitty @ launch ...  # Local fallback
fi
```

## Verification

All findings verified on:
- **Kitty version:** 0.44.0
- **Test date:** 2025-12-17
- **Instances tested:** 4 concurrent
- **Windows tested:** 14 in current, 11 in target
- **Test results:** ✅ All passed

## References

- [Kitty Remote Control Documentation](https://sw.kovidgoyal.net/kitty/remote-control/)
- `kitty @ launch --help`
- `kitty @ --help`
- `man kitty` (--listen-on option)

## Notes

- All file paths are absolute as required
- Socket communication latency: ~1ms
- No performance overhead for cross-instance spawning
- Linear scaling with number of instances
- User isolation via UNIX socket permissions

---

**Last updated:** 2025-12-17 by parallel-worker-5
