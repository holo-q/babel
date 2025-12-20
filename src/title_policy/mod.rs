//! How workers name their conversations to the world
//!
//! Titles are self-expression: when a worker names its conversation, it reveals
//! how it understands its work. The Captain sees these titles in the tower—
//! each one is a worker's public face, their chosen words for the task at hand.
//!
//! Generates "project:task" titles from user prompts via LLM.
//! Titles are buffered and spliced into JSONL on pane close.

mod buffer;
pub mod rolling_prompts;
pub mod splice;

pub use buffer::*;
pub use rolling_prompts::*;
pub use splice::splice_title;

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Instant;

/// Context for title generation
#[derive(Debug, Clone)]
pub struct TitleContext {
    /// Session ID (UUID from JSONL filename)
    pub session_id: String,
    /// Project path
    pub project: PathBuf,
    /// Recent user prompts (chronological, newest last)
    pub recent_prompts: Vec<String>,
    /// Current working directory
    pub cwd: Option<PathBuf>,
    /// Current user message count (for change detection)
    pub user_message_count: usize,
}

/// Generated title with metadata
#[derive(Debug, Clone)]
pub struct GeneratedTitle {
    /// The generated title text
    pub title: String,
    /// Session this belongs to
    pub session_id: String,
    /// When generated
    pub generated_at: Instant,
    /// Source prompts used
    pub source_prompts: Vec<String>,
}

/// Title generation policy trait
///
/// Defines how a worker finds words for its work. Different policies embody
/// different philosophies of self-naming: some workers name from the latest prompt,
/// others distill the essence of an evolving conversation.
#[async_trait]
pub trait TitlePolicy: Send + Sync {
    /// Policy name for logging
    fn name(&self) -> &'static str;

    /// Check if generation should trigger
    fn should_generate(&self, ctx: &TitleContext) -> bool;

    /// Finding words for the work within
    ///
    /// The worker examines its conversation and chooses a name to present
    /// to the tower. This is how it expresses its understanding of the task.
    async fn generate(&self, ctx: TitleContext) -> Result<Option<GeneratedTitle>>;

    /// Cleanup on pane close
    fn on_pane_close(&self, session_id: &str);
}

/// No-op policy for when title generation is disabled
pub struct NoopPolicy;

#[async_trait]
impl TitlePolicy for NoopPolicy {
    fn name(&self) -> &'static str { "noop" }
    fn should_generate(&self, _ctx: &TitleContext) -> bool { false }
    async fn generate(&self, _ctx: TitleContext) -> Result<Option<GeneratedTitle>> { Ok(None) }
    fn on_pane_close(&self, _session_id: &str) {}
}
