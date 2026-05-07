//! Configuration schema for babel
//!
//! Defines the structure of babel.toml with serde-compatible types.
//! All fields provide sensible defaults to allow partial configuration.

use serde::{Deserialize, Serialize};

/// Root configuration for babel daemon
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BabelConfig {
    /// Title policy configuration for generating conversation titles
    pub title_policy: TitlePolicyConfig,
    /// Launch configuration — how babel opens new agent sessions
    pub launch: LaunchConfig,
}

impl Default for BabelConfig {
    fn default() -> Self {
        Self {
            title_policy: TitlePolicyConfig::default(),
            launch: LaunchConfig::default(),
        }
    }
}

/// Configuration for launching new agent sessions (resume, fork, fire).
///
/// By default, babel uses the detected backend (kitty/tmux/zellij) to open
/// panes natively. Set `command` to override with a custom shell command.
///
/// The command template supports these placeholders:
/// - `{cmd}`  — the full agent command (e.g., `claude --resume abc123`)
/// - `{cwd}`  — the working directory for the session
/// - `{args}` — just the arguments (without the binary name)
///
/// Examples:
/// ```toml
/// [launch]
/// # Use tmux to open a new window
/// command = "tmux new-window -c {cwd} {cmd}"
///
/// # Use zellij to open a new pane
/// command = "zellij action new-pane --cwd {cwd} -- {cmd}"
///
/// # Use kitty to open a new OS window
/// command = "kitty --directory {cwd} {cmd}"
///
/// # Use a custom terminal
/// command = "alacritty --working-directory {cwd} -e {cmd}"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LaunchConfig {
    /// Custom launch command template. When set, overrides the backend's
    /// native `launch_pane()`. Placeholders: `{cmd}`, `{cwd}`, `{args}`.
    /// When unset (default), uses the detected backend.
    pub command: Option<String>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self { command: None }
    }
}

/// Configuration for conversation title generation
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TitlePolicyConfig {
    /// Whether title generation is enabled
    #[serde(default = "default_title_policy_enabled")]
    pub enabled: bool,

    /// Policy name (currently only "rolling_prompts" supported)
    #[serde(default = "default_title_policy")]
    pub policy: String,

    /// Configuration for rolling_prompts policy
    pub rolling_prompts: RollingPromptsConfig,

    /// Storage configuration for title persistence
    pub storage: StorageConfig,
}

impl Default for TitlePolicyConfig {
    fn default() -> Self {
        Self {
            enabled: default_title_policy_enabled(),
            policy: default_title_policy(),
            rolling_prompts: RollingPromptsConfig::default(),
            storage: StorageConfig::default(),
        }
    }
}

/// Configuration for the rolling_prompts title generation policy
///
/// This policy uses the N most recent user prompts to generate a coherent
/// "project:task" style title via Claude API inference.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RollingPromptsConfig {
    /// Number of recent prompts to include in title generation
    #[serde(default = "default_prompt_count")]
    pub prompt_count: usize,

    /// Claude model to use for title generation
    #[serde(default = "default_model")]
    pub model: String,

    /// Maximum tokens for the title response
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Debounce delay in seconds before triggering title generation
    ///
    /// After the last user prompt, babel waits this duration before
    /// making the API call. This prevents excessive API usage during
    /// rapid back-and-forth exchanges.
    #[serde(default = "default_debounce_secs")]
    pub debounce_secs: u64,

    /// Prompt template for title generation
    ///
    /// Must include `{prompts}` placeholder where recent prompts will be inserted.
    #[serde(default = "default_prompt_template")]
    pub prompt_template: String,
}

impl Default for RollingPromptsConfig {
    fn default() -> Self {
        Self {
            prompt_count: default_prompt_count(),
            model: default_model(),
            max_tokens: default_max_tokens(),
            debounce_secs: default_debounce_secs(),
            prompt_template: default_prompt_template(),
        }
    }
}

/// Configuration for title storage and persistence
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageConfig {
    /// When to flush title updates to disk
    ///
    /// - "on_close": Write title when conversation closes (minimal I/O)
    /// - "immediate": Write title as soon as it's generated (durable)
    #[serde(default = "default_flush_strategy")]
    pub flush_strategy: String,

    /// Delay in milliseconds before settling JSONL writes
    ///
    /// After updating a conversation's JSONL file, babel waits this duration
    /// to batch multiple rapid updates into a single write operation.
    #[serde(default = "default_jsonl_settle_delay_ms")]
    pub jsonl_settle_delay_ms: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            flush_strategy: default_flush_strategy(),
            jsonl_settle_delay_ms: default_jsonl_settle_delay_ms(),
        }
    }
}

// === Default value functions ===

fn default_title_policy_enabled() -> bool {
    true
}

fn default_title_policy() -> String {
    "rolling_prompts".to_string()
}

fn default_prompt_count() -> usize {
    4
}

fn default_model() -> String {
    "claude-3-5-haiku-latest".to_string()
}

fn default_max_tokens() -> u32 {
    32
}

fn default_debounce_secs() -> u64 {
    5
}

fn default_prompt_template() -> String {
    r#"Generate a "project:task" title from these recent user prompts.
Format: lowercase, colon separator, no quotes (e.g., "babel:title-policy").

The prompts may be:
- Unrelated: Use only the latest prompt for the title
- A stacking sequence: Combine into one coherent work item

Prompts (newest last):
{prompts}

Title:"#
        .to_string()
}

fn default_flush_strategy() -> String {
    "on_close".to_string()
}

fn default_jsonl_settle_delay_ms() -> u64 {
    500
}
