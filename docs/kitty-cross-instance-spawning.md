# Kitty Cross-Instance Window Spawning Research

**Date:** 2025-12-17
**Researcher:** parallel-worker-5
**Status:** Verified and documented

## Summary

Yes, kitty supports spawning windows in specific instances via the `--to` flag. This enables cross-instance window creation by targeting socket addresses.

## Core Mechanism

### Socket-Based Addressing

Each kitty instance listens on a unique socket when configured with `listen_on`:

```bash
# kitty.conf
allow_remote_control true
listen_on unix:$XDG_RUNTIME_DIR/kitty.sock
```

This creates instance-specific sockets like:
- `/run/user/1000/kitty.sock-3497`
- `/run/user/1000/kitty.sock-4690`
- `/run/user/1000/kitty.sock-73096`
- `/run/user/1000/kitty.sock-74830`

### The `--to` Flag

The `--to` global option targets a specific kitty instance by socket address:

```
--to
  An address for the kitty instance to control. Corresponds to the address
  given to the kitty instance via the --listen-on option or the listen_on
  setting in kitty.conf. If not specified, the environment variable
  KITTY_LISTEN_ON is checked. If that is also not found, messages are sent to
  the controlling terminal for this process, i.e. they will only work if this
  process is run within a kitty window.
```

## Cross-Instance Window Spawning

### Basic Syntax

```bash
kitty @ --to unix:/path/to/socket launch [OPTIONS] [COMMAND]
```

### Examples

#### List windows in a specific instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-4690 ls
```

#### Spawn a new window in instance 4690

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-4690 launch --title "Remote Window" bash
```

#### Spawn a new tab in instance 73096

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-73096 launch --type tab --title "Remote Tab"
```

#### Spawn a new OS window in instance 3497

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-3497 launch --type os-window
```

## Resolution Hierarchy

When no `--to` is specified, kitty resolves the target in this order:

1. **`--to` flag** (highest priority)
2. **`KITTY_LISTEN_ON` environment variable**
3. **Controlling terminal** (only works when run inside a kitty window)

This means:
- Running `kitty @ launch` inside a kitty window targets **that** instance
- Running with `KITTY_LISTEN_ON` set targets that socket
- Running with `--to unix:/path` targets the specified instance **regardless of context**

## Use Cases for Cross-Instance Spawning

### 1. Centralized Window Management

A daemon or script can spawn windows in specific kitty instances based on context:

```bash
# Spawn development tools in workspace 1's kitty
kitty @ --to unix:/run/user/1000/kitty.sock-1234 launch htop

# Spawn monitoring in workspace 2's kitty
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch journalctl -f
```

### 2. Session Restoration

Restore windows to specific kitty instances after logout:

```bash
for socket in /run/user/1000/kitty.sock-*; do
  kitty @ --to unix:$socket launch --cwd ~/project nvim
done
```

### 3. Multi-Monitor Workflows

Target kitty instances on different monitors:

```bash
# Left monitor (instance 1234)
kitty @ --to unix:/run/user/1000/kitty.sock-1234 launch code

# Right monitor (instance 5678)
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch --type tab firefox
```

### 4. Remote Control from Non-Kitty Terminals

Spawn windows in kitty from any terminal (xterm, alacritty, etc.):

```bash
# From any terminal
kitty @ --to unix:/run/user/1000/kitty.sock-3497 launch claude
```

## Advanced: Dynamic Instance Discovery

### List All Running Instances

```bash
ls -1 /run/user/1000/kitty.sock-*
```

### Get Instance PIDs

```bash
# Socket name format: kitty.sock-{PID}
for sock in /run/user/1000/kitty.sock-*; do
  pid=$(basename "$sock" | cut -d- -f2)
  echo "Instance $pid: $sock"
done
```

### Get Window Counts per Instance

```bash
for sock in /run/user/1000/kitty.sock-*; do
  count=$(kitty @ --to unix:$sock ls 2>/dev/null | grep -c '"id":')
  echo "$(basename $sock): $count windows"
done
```

## Limitations and Gotchas

### 1. Remote Control Must Be Enabled

Each instance must have `allow_remote_control` enabled:

```conf
# kitty.conf
allow_remote_control yes
```

Or started with:

```bash
kitty -o allow_remote_control=yes
```

### 2. Socket Permissions

Sockets are user-specific (`/run/user/UID/`). Cross-user control requires:
- Socket permissions modification (not recommended)
- Running as the target user

### 3. No Automatic Instance Discovery

There's no built-in way to list instances. You must:
- Track sockets manually (`ls /run/user/1000/kitty.sock-*`)
- Use a registry/database of known instances
- Query via IPC mechanisms

### 4. Socket Cleanup

Stale sockets may persist after crashes. Clean manually:

```bash
# Remove sockets for dead PIDs
for sock in /run/user/1000/kitty.sock-*; do
  pid=$(basename "$sock" | cut -d- -f2)
  if ! kill -0 "$pid" 2>/dev/null; then
    rm "$sock"
  fi
done
```

### 5. KITTY_LISTEN_ON in Spawned Windows

Windows spawned via `--to` inherit `KITTY_LISTEN_ON` from the **target** instance, not the caller:

```bash
# Caller has KITTY_LISTEN_ON=unix:/run/user/1000/kitty.sock-1234
# Spawning in instance 5678:
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch bash

# The new bash shell has KITTY_LISTEN_ON=unix:/run/user/1000/kitty.sock-5678
# NOT kitty.sock-1234
```

This is correct behavior—nested `kitty @` calls target the parent instance.

## Integration with claude-babel

### Routing Windows to Daemon's Kitty Instance

If the daemon runs in instance `sock-9999`, route windows there:

```bash
DAEMON_SOCK="unix:/run/user/1000/kitty.sock-9999"

# Spawn output window in daemon's kitty
kitty @ --to "$DAEMON_SOCK" launch --type overlay \
  --title "Claude Output" \
  --cwd "$project_dir" \
  bash -c "tail -f output.log"
```

### Discovering Daemon's Instance

Store the daemon's PID and derive the socket:

```bash
# On daemon startup
echo $KITTY_PID > /run/user/1000/claude-babel-kitty.pid

# When routing
daemon_pid=$(cat /run/user/1000/claude-babel-kitty.pid)
daemon_sock="unix:/run/user/1000/kitty.sock-$daemon_pid"
kitty @ --to "$daemon_sock" launch ...
```

### Fallback to Local Instance

If daemon socket unavailable, spawn locally:

```bash
daemon_sock="unix:/run/user/1000/kitty.sock-9999"

if kitty @ --to "$daemon_sock" ls &>/dev/null; then
  kitty @ --to "$daemon_sock" launch ...
else
  kitty @ launch ...  # Spawn in current/local instance
fi
```

## Verification Commands

### Test Cross-Instance Spawning

```bash
# Get current instance socket
current_sock=$KITTY_LISTEN_ON

# List all instances
all_socks=($(ls /run/user/1000/kitty.sock-*))

# Find a different instance
for sock in "${all_socks[@]}"; do
  if [[ "$sock" != "${current_sock#unix:}" ]]; then
    target_sock="unix:$sock"
    break
  fi
done

# Spawn in the other instance
kitty @ --to "$target_sock" launch --title "Cross-Instance Test" bash
```

### Verify Window Appeared in Target

```bash
# List windows in target instance
kitty @ --to "$target_sock" ls | grep "Cross-Instance Test"
```

## Conclusion

**Yes, kitty fully supports cross-instance window spawning via `--to`.**

Key takeaways:
1. Use `--to unix:/path/to/socket` to target specific instances
2. Works with all `kitty @` commands (launch, close-window, set-colors, etc.)
3. Socket paths follow pattern: `/run/user/UID/kitty.sock-{PID}`
4. Requires `allow_remote_control` in target instance
5. Resolution order: `--to` > `KITTY_LISTEN_ON` > controlling terminal

This enables powerful workflows like:
- Centralized window management across workspaces
- Daemon-controlled UI spawning
- Multi-monitor session orchestration
- Remote control from any terminal emulator

## References

- [Kitty Remote Control Documentation](https://sw.kovidgoyal.net/kitty/remote-control/)
- `kitty @ launch --help`
- `man kitty` (--listen-on option)
- Verified on kitty 0.44.0
