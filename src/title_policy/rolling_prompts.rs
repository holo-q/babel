//! Distilling the conversation's essence into a name
//!
//! The worker gathers its recent prompts—the living context of its task—and
//! asks Haiku to help find words. Are these prompts a sequence building toward
//! one goal? Or unrelated threads where only the latest matters? The policy
//! embodies the worker's process of self-naming from the flow of its work.
//!
//! Takes last N user prompts and generates "project:task" titles via Haiku.
//! Designed to handle both unrelated prompts (use latest) and stacking sequences
//! that build toward a single work item.

use super::{GeneratedTitle, TitleContext, TitlePolicy};
use crate::babel_storage::{init_db, set_generated_title};
use crate::config::RollingPromptsConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

/// Distilling the conversation's evolving essence into a name
///
/// A worker using this policy looks back at its recent prompts to find its name.
/// It gathers the last N exchanges and asks Haiku: "What is the through-line here?"
/// Haiku sees whether the prompts build on each other or stand alone, then offers
/// a title—the worker's chosen expression of its work to the tower.
///
/// This policy:
/// 1. Collects the last N user prompts from the conversation
/// 2. Sends them to Haiku with a prompt template
/// 3. Haiku decides if they're related (combine) or unrelated (use latest)
/// 4. Returns a "project:task" formatted title
pub struct RollingPromptsPolicy {
    config: RollingPromptsConfig,
    client: reqwest::Client,
    api_key: Option<String>,
    /// Last generation time per session (debounce)
    last_gen: RwLock<HashMap<String, Instant>>,
    /// Last user message count per session (change detection)
    last_count: RwLock<HashMap<String, usize>>,
}

impl RollingPromptsPolicy {
    pub fn new(config: RollingPromptsConfig) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        if api_key.is_none() {
            tracing::warn!("ANTHROPIC_API_KEY not set, title generation disabled");
        }

        Self {
            config,
            client: reqwest::Client::new(),
            api_key,
            last_gen: RwLock::new(HashMap::new()),
            last_count: RwLock::new(HashMap::new()),
        }
    }

    /// Check if API is available
    pub fn is_enabled(&self) -> bool {
        self.api_key.is_some()
    }

    /// Generate a title directly from already-collected user prompts.
    ///
    /// Manual session-title refresh uses this path after the harness has read
    /// durable transcript prompts and before it writes the harness-native title
    /// record. It deliberately skips debounce/session bookkeeping: explicit
    /// refresh is a direct command, not ambient daemon policy.
    pub async fn generate_from_prompts(&self, prompts: &[String]) -> Result<Option<String>> {
        let Some(api_key) = self.api_key.as_deref() else {
            return Ok(None);
        };

        let prompts = prompts
            .iter()
            .rev()
            .take(self.config.prompt_count)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        if prompts.is_empty() {
            return Ok(None);
        }

        self.call_haiku(api_key, &prompts).await.map(Some)
    }

    /// Call Haiku API to generate title
    async fn call_haiku(&self, api_key: &str, prompts: &[String]) -> Result<String> {
        // Format prompts for template (numbered, newest last)
        let prompts_text = prompts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // Truncate long prompts to avoid excessive token usage
                let truncated = if p.len() > 200 {
                    format!("{}...", &p[..197])
                } else {
                    p.clone()
                };
                format!("{}. {}", i + 1, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Substitute into template
        let prompt = self
            .config
            .prompt_template
            .replace("{prompts}", &prompts_text);

        tracing::debug!(
            prompt_count = prompts.len(),
            template_len = prompt.len(),
            "Calling Haiku for title generation"
        );

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": &self.config.model,
                "max_tokens": self.config.max_tokens,
                "messages": [
                    {"role": "user", "content": prompt}
                ]
            }))
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {}: {}", status, body);
        }

        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        // Extract text from response
        let title = body["content"][0]["text"]
            .as_str()
            .unwrap_or("untitled")
            .trim()
            .to_lowercase()
            // Clean up any quotes or trailing punctuation
            .trim_matches(|c| c == '"' || c == '\'' || c == '.' || c == ',')
            .to_string();

        tracing::debug!(title = %title, "Haiku generated title");

        Ok(title)
    }
}

#[async_trait]
impl TitlePolicy for RollingPromptsPolicy {
    fn name(&self) -> &'static str {
        "rolling_prompts"
    }

    fn should_generate(&self, ctx: &TitleContext) -> bool {
        // No API key = no generation
        if self.api_key.is_none() {
            return false;
        }

        // Need at least one prompt
        if ctx.recent_prompts.is_empty() {
            return false;
        }

        // Skip agent sessions
        if ctx.session_id.starts_with("agent-") {
            return false;
        }

        // Check if message count increased (new user prompt)
        {
            let last_count = self.last_count.read().unwrap();
            if let Some(&last) = last_count.get(&ctx.session_id) {
                if ctx.user_message_count <= last {
                    // No new messages
                    return false;
                }
            }
        }

        // Debounce check
        {
            let last_gen = self.last_gen.read().unwrap();
            if let Some(last) = last_gen.get(&ctx.session_id) {
                if last.elapsed().as_secs() < self.config.debounce_secs {
                    tracing::trace!(
                        session_id = %ctx.session_id,
                        elapsed = ?last.elapsed(),
                        debounce = self.config.debounce_secs,
                        "Debounced title generation"
                    );
                    return false;
                }
            }
        }

        true
    }

    async fn generate(&self, ctx: TitleContext) -> Result<Option<GeneratedTitle>> {
        let api_key = match &self.api_key {
            Some(k) => k,
            None => {
                tracing::debug!("No API key, skipping title generation");
                return Ok(None);
            }
        };

        // Take last N prompts (configured prompt_count)
        // The worker gathers its recent context to understand what it's been doing
        let prompts: Vec<String> = ctx
            .recent_prompts
            .iter()
            .rev()
            .take(self.config.prompt_count)
            .rev()
            .cloned()
            .collect();

        if prompts.is_empty() {
            return Ok(None);
        }

        tracing::info!(
            session_id = %ctx.session_id,
            prompt_count = prompts.len(),
            "Worker finding words for its work"
        );

        // Ask Haiku to help distill the essence into a name
        let title = match self.call_haiku(api_key, &prompts).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    session_id = %ctx.session_id,
                    error = %e,
                    "Failed to generate title"
                );
                return Ok(None);
            }
        };

        // Persist to babel's overlay DB so we can distinguish haiku titles from procedural
        // This enables babel ls to style titles: haiku=normal, procedural=dim+italic
        if let Ok(conn) = init_db() {
            if let Err(e) = set_generated_title(&conn, &ctx.session_id, &title) {
                tracing::warn!(
                    session_id = %ctx.session_id,
                    error = %e,
                    "Failed to persist generated title to overlay DB"
                );
            } else {
                tracing::debug!(
                    session_id = %ctx.session_id,
                    title = %title,
                    "Persisted haiku-generated title to overlay DB"
                );
            }
        }

        // Update tracking state
        {
            let mut last_gen = self.last_gen.write().unwrap();
            last_gen.insert(ctx.session_id.clone(), Instant::now());
        }
        {
            let mut last_count = self.last_count.write().unwrap();
            last_count.insert(ctx.session_id.clone(), ctx.user_message_count);
        }

        Ok(Some(GeneratedTitle {
            title,
            session_id: ctx.session_id,
            generated_at: Instant::now(),
            source_prompts: prompts,
        }))
    }

    fn on_pane_close(&self, session_id: &str) {
        // Cleanup tracking state
        self.last_gen.write().unwrap().remove(session_id);
        self.last_count.write().unwrap().remove(session_id);
        tracing::trace!(session_id, "Cleaned up title policy state on pane close");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RollingPromptsConfig;

    fn make_test_config() -> RollingPromptsConfig {
        RollingPromptsConfig {
            prompt_count: 4,
            model: "claude-3-5-haiku-latest".to_string(),
            max_tokens: 32,
            debounce_secs: 5,
            prompt_template: "Test template: {prompts}".to_string(),
        }
    }

    #[test]
    fn test_should_generate_no_api_key() {
        // Clear API key for test
        std::env::remove_var("ANTHROPIC_API_KEY");

        let policy = RollingPromptsPolicy::new(make_test_config());
        let ctx = TitleContext {
            session_id: "test-session".to_string(),
            project: std::path::PathBuf::from("/test"),
            recent_prompts: vec!["hello".to_string()],
            cwd: None,
            user_message_count: 1,
        };

        assert!(!policy.should_generate(&ctx));
    }

    #[test]
    fn test_should_generate_no_prompts() {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");

        let policy = RollingPromptsPolicy::new(make_test_config());
        let ctx = TitleContext {
            session_id: "test-session".to_string(),
            project: std::path::PathBuf::from("/test"),
            recent_prompts: vec![],
            cwd: None,
            user_message_count: 0,
        };

        assert!(!policy.should_generate(&ctx));

        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn test_should_generate_agent_session() {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");

        let policy = RollingPromptsPolicy::new(make_test_config());
        let ctx = TitleContext {
            session_id: "agent-12345".to_string(),
            project: std::path::PathBuf::from("/test"),
            recent_prompts: vec!["hello".to_string()],
            cwd: None,
            user_message_count: 1,
        };

        assert!(!policy.should_generate(&ctx));

        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
