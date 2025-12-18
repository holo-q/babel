# Kitty Window Routing - Quick Reference

## TL;DR

**Yes, you can spawn windows in specific kitty instances using `--to`.**

```bash
# Spawn in instance with PID 1234
kitty @ --to unix:/run/user/$(id -u)/kitty.sock-1234 launch [OPTIONS] [COMMAND]
```

## Socket Path Pattern

```
/run/user/{UID}/kitty.sock-{PID}
```

Example:
- PID: 5678
- UID: 1000
- Socket: `/run/user/1000/kitty.sock-5678`

## Common Operations

### List All Instances

```bash
ls -1 /run/user/$(id -u)/kitty.sock-*
```

### Get Window Count for Instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 ls | grep -c '"id":'
```

### Spawn Window in Specific Instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch \
  --title "My Window" \
  --cwd /path/to/dir \
  bash
```

### Spawn Tab in Specific Instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch \
  --type tab \
  --title "My Tab" \
  nvim
```

### Spawn Background Process in Instance

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 launch \
  --type background \
  my-daemon
```

## Resolution Order

When `kitty @` resolves which instance to target:

1. **`--to` flag** ← highest priority (explicit override)
2. **`KITTY_LISTEN_ON` env var** ← set by parent instance
3. **Controlling terminal** ← only works inside kitty window

## claude-babel Integration Pattern

### 1. Store Daemon's Kitty Socket on Startup

```rust
// On daemon startup (inside kitty window)
let socket = std::env::var("KITTY_LISTEN_ON")?;
std::fs::write("/run/user/1000/claude-babel-kitty.sock", socket)?;
```

### 2. Route Windows to Daemon's Instance

```rust
fn spawn_in_daemon_kitty(cmd: &str) -> Result<()> {
    let socket = std::fs::read_to_string("/run/user/1000/claude-babel-kitty.sock")?;

    Command::new("kitty")
        .args(&["@", "--to", &socket, "launch"])
        .args(&["--title", "Claude Output"])
        .arg(cmd)
        .spawn()?;

    Ok(())
}
```

### 3. Fallback to Local Instance

```rust
fn spawn_window_with_fallback(cmd: &str) -> Result<()> {
    let daemon_socket = std::fs::read_to_string("/run/user/1000/claude-babel-kitty.sock")
        .ok();

    let mut launch = Command::new("kitty");
    launch.arg("@");

    // Route to daemon if available, otherwise use current instance
    if let Some(socket) = daemon_socket {
        if is_socket_alive(&socket) {
            launch.args(&["--to", &socket]);
        }
    }

    launch.args(&["launch", "--title", "Claude Output", cmd]).spawn()?;
    Ok(())
}

fn is_socket_alive(socket: &str) -> bool {
    Command::new("kitty")
        .args(&["@", "--to", socket, "ls"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}
```

## Environment Variable Inheritance

**Critical:** Windows spawned via `--to` inherit `KITTY_LISTEN_ON` from the **target** instance, not the caller.

```bash
# Caller has KITTY_LISTEN_ON=unix:/run/user/1000/kitty.sock-1111
# Spawning in instance 2222:
kitty @ --to unix:/run/user/1000/kitty.sock-2222 launch bash

# The new bash has KITTY_LISTEN_ON=unix:/run/user/1000/kitty.sock-2222
```

This is correct—nested `kitty @` calls target the parent instance.

## Debugging

### Check Remote Control is Enabled

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 ls
```

If this fails, check `kitty.conf`:

```conf
allow_remote_control yes
```

### Verify Socket Exists

```bash
ls -l /run/user/1000/kitty.sock-5678
```

### Check Process is Alive

```bash
kill -0 5678  # Returns 0 if process exists
```

### Clean Stale Sockets

```bash
for sock in /run/user/1000/kitty.sock-*; do
    pid=$(basename "$sock" | cut -d- -f2)
    if ! kill -0 "$pid" 2>/dev/null; then
        rm "$sock"
        echo "Removed stale socket: $sock"
    fi
done
```

## Security Considerations

### User Isolation

Sockets are user-specific (`/run/user/{UID}/`). Users cannot control other users' kitty instances without:
- Modifying socket permissions (dangerous)
- Running as the target user (`sudo -u`)

### Password Protection

For sensitive operations, use `--password`:

```bash
kitty @ --to unix:/run/user/1000/kitty.sock-5678 \
    --password "secret" \
    launch sensitive-command
```

Configure in `kitty.conf`:

```conf
remote_control_password "secret" launch close-window
```

## Performance Notes

- Socket communication is fast (~1ms latency)
- No overhead for routing vs. local spawn
- Background processes (`--type background`) have minimal footprint
- Multiple instances scale linearly (no shared state)

## Tested Scenarios

✅ Spawn window in different instance
✅ Spawn tab in different instance
✅ Spawn background process in different instance
✅ Verify window exists via `ls` in target instance
✅ Close windows remotely in other instances
✅ Get window counts per instance
✅ Fallback to local instance when daemon unavailable

## References

- Full documentation: `/home/nuck/Workspace/Daemons/claude-babel/docs/kitty-cross-instance-spawning.md`
- Test script: `/home/nuck/Workspace/Daemons/claude-babel/tools/test-cross-instance-spawn.sh`
- Kitty docs: https://sw.kovidgoyal.net/kitty/remote-control/
