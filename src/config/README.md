# Babel Configuration Module

Configuration management for the babel daemon, with graceful fallback to sensible defaults.

## File Location

Configuration is loaded from `~/.config/babel/babel.toml`. If the file doesn't exist, babel runs with built-in defaults.

## Usage

```rust
use babel::config::load_config;

let config = load_config()?;
if config.title_policy.enabled {
    println!("Title policy: {}", config.title_policy.policy);
}
```

## Example Configuration

Run `cargo run --example show_config` to see the full default configuration, or refer to this minimal example:

```toml
[title_policy]
enabled = true
policy = "rolling_prompts"

[title_policy.rolling_prompts]
prompt_count = 4
model = "claude-3-5-haiku-latest"
max_tokens = 32
debounce_secs = 5
prompt_template = """
Generate a "project:task" title from these recent user prompts.
Format: lowercase, colon separator, no quotes (e.g., "babel:title-policy").

The prompts may be:
- Unrelated: Use only the latest prompt for the title
- A stacking sequence: Combine into one coherent work item

Prompts (newest last):
{prompts}

Title:"""

[title_policy.storage]
flush_strategy = "on_close"
jsonl_settle_delay_ms = 500
```

## Configuration Sections

### `[title_policy]`

Controls conversation title generation.

- **`enabled`** (bool): Enable/disable title generation. Default: `true`
- **`policy`** (string): Policy name. Currently only `"rolling_prompts"` supported. Default: `"rolling_prompts"`

### `[title_policy.rolling_prompts]`

Configuration for the rolling_prompts policy, which uses recent user prompts to generate coherent titles.

- **`prompt_count`** (usize): Number of recent prompts to include. Default: `5`
- **`first_prompt_count`** (usize): Opening prompts to prepend for session ambience. Default: `2`
- **`title_history_count`** (usize): Prior local/native titles to include for naming ambience. Default: `4`
- **`model`** (string): Claude model for title generation. Default: `"claude-3-5-haiku-latest"`
- **`max_tokens`** (u32): Maximum tokens in response. Default: `32`
- **`debounce_secs`** (u64): Seconds to wait after last prompt before generating title. Default: `5`
- **`prompt_template`** (string): Template with `{prompts}` and optional `{titles}` placeholders. See example above.

### `[title_policy.storage]`

Controls when and how titles are persisted to disk.

- **`flush_strategy`** (string): When to write titles. Options:
  - `"on_close"`: Write when conversation closes (minimal I/O, default)
  - `"immediate"`: Write as soon as title is generated (durable)
- **`jsonl_settle_delay_ms`** (u64): Milliseconds to wait before settling JSONL writes, allowing batching of rapid updates. Default: `500`

## Hot Reload

Configuration changes can be applied by restarting the babel daemon:

```bash
systemctl --user restart babel.service
```

Future versions may support live reload via file watching.

## Validation

The module includes tests to ensure:
- Default config can round-trip through TOML serialization
- Missing config files are handled gracefully
- Config path resolution works correctly

Run tests with:

```bash
cargo test --lib config
```
