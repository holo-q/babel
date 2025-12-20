# claude-babel - Claude Code IPC daemon

# Install babel binary to ~/.cargo/bin
install:
    cargo install --path .

# Build release (without installing)
build:
    cargo build --release

# Run tests
test:
    cargo test

# Check types
check:
    cargo check

# Clean build artifacts
clean:
    cargo clean
