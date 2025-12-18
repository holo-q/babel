#!/usr/bin/env bash
# Utility script for discovering and managing kitty instances
# Helps with cross-instance window spawning and debugging

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

usage() {
    cat <<EOF
Usage: $(basename "$0") [COMMAND] [OPTIONS]

Commands:
    list            List all running kitty instances (default)
    sockets         List socket paths only
    current         Show current instance socket
    pids            List PIDs only
    info <PID>      Show detailed info for specific instance
    spawn <PID>     Spawn test window in specific instance
    clean           Remove stale sockets for dead processes

Options:
    -h, --help      Show this help message
    -v, --verbose   Verbose output

Examples:
    $(basename "$0") list              # List all instances
    $(basename "$0") info 1234         # Show info for instance 1234
    $(basename "$0") spawn 1234        # Spawn test window in instance 1234
    $(basename "$0") sockets           # Get socket paths (for scripts)

EOF
    exit 0
}

list_instances() {
    local verbose=${1:-false}
    local sockets=(/run/user/$(id -u)/kitty.sock-*)

    if [[ ${#sockets[@]} -eq 0 ]] || [[ ! -e "${sockets[0]}" ]]; then
        echo -e "${YELLOW}No kitty instances found${NC}"
        return 1
    fi

    echo -e "${CYAN}Kitty Instances (${#sockets[@]} total):${NC}"
    echo ""

    local current_sock="${KITTY_LISTEN_ON#unix:}"

    for sock in "${sockets[@]}"; do
        local pid=$(basename "$sock" | cut -d- -f2)
        local window_count=$(kitty @ --to "unix:$sock" ls 2>/dev/null | grep -c '"id":' || echo "?")
        local tab_count=$(kitty @ --to "unix:$sock" ls 2>/dev/null | grep -c '"tabs":' || echo "?")

        local marker=""
        if [[ "$sock" == "$current_sock" ]]; then
            marker=" ${GREEN}(current)${NC}"
        fi

        if [[ "$verbose" == "true" ]]; then
            echo -e "${BLUE}PID ${pid}${NC}$marker"
            echo "  Socket: $sock"
            echo "  Windows: $window_count"
            echo "  Tabs: $tab_count"

            # Get focused window title
            local focused_title=$(kitty @ --to "unix:$sock" ls 2>/dev/null | \
                grep -A 5 '"is_focused": true' | \
                grep '"title"' | \
                head -1 | \
                sed 's/.*"title": "\(.*\)".*/\1/' || echo "none")
            echo "  Focused: $focused_title"
            echo ""
        else
            printf "  ${BLUE}%-8s${NC} %s  ${YELLOW}%2s${NC} windows  ${CYAN}%2s${NC} tabs%b\n" \
                "PID $pid" "$sock" "$window_count" "$tab_count" "$marker"
        fi
    done
}

list_sockets() {
    local sockets=(/run/user/$(id -u)/kitty.sock-*)

    if [[ ${#sockets[@]} -eq 0 ]] || [[ ! -e "${sockets[0]}" ]]; then
        return 1
    fi

    for sock in "${sockets[@]}"; do
        echo "$sock"
    done
}

list_pids() {
    local sockets=(/run/user/$(id -u)/kitty.sock-*)

    if [[ ${#sockets[@]} -eq 0 ]] || [[ ! -e "${sockets[0]}" ]]; then
        return 1
    fi

    for sock in "${sockets[@]}"; do
        basename "$sock" | cut -d- -f2
    done
}

show_current() {
    if [[ -z "${KITTY_LISTEN_ON:-}" ]]; then
        echo -e "${YELLOW}Not running inside a kitty window${NC}"
        return 1
    fi

    local sock="${KITTY_LISTEN_ON#unix:}"
    local pid=$(basename "$sock" | cut -d- -f2)

    echo -e "${GREEN}Current Instance:${NC}"
    echo "  PID: $pid"
    echo "  Socket: $sock"
    echo "  Env: $KITTY_LISTEN_ON"
}

show_info() {
    local pid=$1
    local sock="/run/user/$(id -u)/kitty.sock-$pid"

    if [[ ! -e "$sock" ]]; then
        echo -e "${RED}No instance found with PID $pid${NC}"
        return 1
    fi

    echo -e "${CYAN}Instance Info (PID $pid):${NC}"
    echo ""
    echo "Socket: $sock"
    echo ""

    # Get full ls output
    echo "Windows:"
    kitty @ --to "unix:$sock" ls 2>/dev/null | grep -E '"title":|"id":' | \
        sed 's/.*"title": "\(.*\)".*/  - \1/' | \
        sed 's/.*"id": \([0-9]*\).*/    (ID: \1)/' || echo "  (none)"
}

spawn_test_window() {
    local pid=$1
    local sock="/run/user/$(id -u)/kitty.sock-$pid"

    if [[ ! -e "$sock" ]]; then
        echo -e "${RED}No instance found with PID $pid${NC}"
        return 1
    fi

    echo -e "${BLUE}Spawning test window in instance $pid...${NC}"

    local window_id=$(kitty @ --to "unix:$sock" launch \
        --title "Test Window (from kitty-instances.sh)" \
        --dont-take-focus \
        bash -c 'echo "Test window spawned at $(date)"; echo "Target PID: '"$pid"'"; echo "Press Enter to close..."; read')

    if [[ -n "$window_id" ]]; then
        echo -e "${GREEN}Success!${NC} Window ID: $window_id"
    else
        echo -e "${RED}Failed to spawn window${NC}"
        return 1
    fi
}

clean_stale_sockets() {
    local sockets=(/run/user/$(id -u)/kitty.sock-*)
    local removed=0

    if [[ ${#sockets[@]} -eq 0 ]] || [[ ! -e "${sockets[0]}" ]]; then
        echo -e "${YELLOW}No sockets found${NC}"
        return 0
    fi

    echo -e "${CYAN}Checking for stale sockets...${NC}"

    for sock in "${sockets[@]}"; do
        local pid=$(basename "$sock" | cut -d- -f2)

        if ! kill -0 "$pid" 2>/dev/null; then
            echo -e "${YELLOW}Removing stale socket:${NC} $sock (PID $pid is dead)"
            rm "$sock" || echo -e "${RED}Failed to remove $sock${NC}"
            ((removed++))
        fi
    done

    if [[ $removed -eq 0 ]]; then
        echo -e "${GREEN}No stale sockets found${NC}"
    else
        echo -e "${GREEN}Removed $removed stale socket(s)${NC}"
    fi
}

# Parse arguments
verbose=false
command="list"

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            ;;
        -v|--verbose)
            verbose=true
            shift
            ;;
        list|sockets|current|pids|info|spawn|clean)
            command="$1"
            shift
            break
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            usage
            ;;
    esac
done

# Execute command
case "$command" in
    list)
        list_instances "$verbose"
        ;;
    sockets)
        list_sockets
        ;;
    pids)
        list_pids
        ;;
    current)
        show_current
        ;;
    info)
        if [[ -z "${1:-}" ]]; then
            echo -e "${RED}Error: PID required${NC}"
            echo "Usage: $(basename "$0") info <PID>"
            exit 1
        fi
        show_info "$1"
        ;;
    spawn)
        if [[ -z "${1:-}" ]]; then
            echo -e "${RED}Error: PID required${NC}"
            echo "Usage: $(basename "$0") spawn <PID>"
            exit 1
        fi
        spawn_test_window "$1"
        ;;
    clean)
        clean_stale_sockets
        ;;
esac
