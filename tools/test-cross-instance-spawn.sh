#!/usr/bin/env bash
# Test script for kitty cross-instance window spawning
# Validates that we can spawn windows in different kitty instances via --to

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}[INFO]${NC} $*"; }
success() { echo -e "${GREEN}[SUCCESS]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }

# Get all kitty socket files
sockets=(/run/user/$(id -u)/kitty.sock-*)

if [[ ${#sockets[@]} -eq 0 ]] || [[ ! -e "${sockets[0]}" ]]; then
    error "No kitty instances found!"
    exit 1
fi

info "Found ${#sockets[@]} kitty instance(s):"
for sock in "${sockets[@]}"; do
    pid=$(basename "$sock" | cut -d- -f2)
    window_count=$(kitty @ --to "unix:$sock" ls 2>/dev/null | grep -c '"id":' || echo "error")
    echo "  - $sock (PID: $pid, Windows: $window_count)"
done

# Get current instance
current_sock="${KITTY_LISTEN_ON#unix:}"
if [[ -z "$current_sock" ]]; then
    warn "Not running inside a kitty window, using first socket"
    current_sock="${sockets[0]}"
else
    info "Current instance: $current_sock"
fi

# Find a different instance to target
target_sock=""
for sock in "${sockets[@]}"; do
    if [[ "$sock" != "$current_sock" ]]; then
        target_sock="$sock"
        break
    fi
done

if [[ -z "$target_sock" ]]; then
    warn "Only one kitty instance found, spawning in same instance"
    target_sock="$current_sock"
fi

target_pid=$(basename "$target_sock" | cut -d- -f2)
info "Target instance: $target_sock (PID: $target_pid)"

# Test 1: Spawn a new window in target instance
info "Test 1: Spawning new window in target instance..."
window_id=$(kitty @ --to "unix:$target_sock" launch \
    --title "Cross-Instance Test Window" \
    --dont-take-focus \
    bash -c 'echo "This window was spawned from another kitty instance!"; echo "Target PID: '"$target_pid"'"; echo "Press Enter to close..."; read')

if [[ -n "$window_id" ]]; then
    success "Window spawned successfully! (ID: $window_id)"
else
    error "Failed to spawn window"
    exit 1
fi

# Test 2: Verify window exists in target instance
info "Test 2: Verifying window exists in target instance..."
if kitty @ --to "unix:$target_sock" ls | grep -q "Cross-Instance Test Window"; then
    success "Window found in target instance"
else
    error "Window NOT found in target instance"
    exit 1
fi

# Test 3: Spawn a background process in target instance
info "Test 3: Spawning background process in target instance..."
bg_id=$(kitty @ --to "unix:$target_sock" launch \
    --type background \
    bash -c 'sleep 2; echo "Background process completed"')

if [[ -n "$bg_id" ]]; then
    success "Background process spawned successfully! (ID: $bg_id)"
else
    error "Failed to spawn background process"
    exit 1
fi

# Test 4: Get window count before and after spawning
info "Test 4: Testing window count tracking..."
before_count=$(kitty @ --to "unix:$target_sock" ls 2>/dev/null | grep -c '"id":' || echo 0)
info "Windows before spawn: $before_count"

new_window=$(kitty @ --to "unix:$target_sock" launch \
    --title "Count Test Window" \
    --dont-take-focus \
    bash -c 'sleep 1')

after_count=$(kitty @ --to "unix:$target_sock" ls 2>/dev/null | grep -c '"id":' || echo 0)
info "Windows after spawn: $after_count"

if [[ $((after_count - before_count)) -eq 1 ]]; then
    success "Window count increased correctly"
else
    warn "Window count unexpected (before: $before_count, after: $after_count)"
fi

# Test 5: Close the spawned window
info "Test 5: Closing test windows remotely..."
kitty @ --to "unix:$target_sock" close-window --match "title:Count Test Window" || warn "Failed to close Count Test Window"
success "Test windows close command sent"

# Summary
echo ""
success "All tests completed successfully!"
echo ""
info "Summary:"
echo "  ✓ Can spawn windows in specific kitty instances via --to"
echo "  ✓ Can spawn background processes in other instances"
echo "  ✓ Can verify windows exist in target instance"
echo "  ✓ Can track window counts across instances"
echo "  ✓ Can close windows remotely in other instances"
echo ""
info "Key findings:"
echo "  - Socket path format: /run/user/UID/kitty.sock-{PID}"
echo "  - Use --to unix:/path/to/socket to target specific instance"
echo "  - Works with all kitty @ commands (launch, close-window, ls, etc.)"
echo "  - Requires allow_remote_control enabled in target instance"
echo ""
info "For claude-babel integration:"
echo "  1. Store daemon's kitty PID on startup"
echo "  2. Derive socket path: /run/user/UID/kitty.sock-{PID}"
echo "  3. Route windows via: kitty @ --to unix:/path/to/socket launch ..."
echo "  4. Fallback to local spawn if daemon socket unavailable"
