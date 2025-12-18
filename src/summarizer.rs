//! Workspace title summarization via Haiku
//!
//! Generates ambient workspace titles by summarizing active Claude sessions.
//! Uses Claude Haiku for fast, cheap summarization with caching and debouncing.
//!
//! ## Usage
//!
//! The summarizer is called when:
//! - A window is added/removed from a workspace
//! - A session is matched to a window
//! - Periodically for active workspaces (TTL refresh)
//!
//! ## Output
//!
//! Produces 2-5 word titles like:
//! - "refactoring auth system"
//! - "debugging API endpoints"
//! - "3 parallel workers"

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Cached workspace title
#[derive(Debug, Clone)]
struct CachedTitle {
    title: String,
    generated_at: Instant,
    /// Hash of input sessions to detect changes
    input_hash: u64,
}

/// Session info for summarization
#[derive(Debug, Clone)]
pub struct SessionSummaryInput {
    pub project_path: String,
    pub recent_activity: Option<String>,
    pub window_title: Option<String>,
}

/// Workspace summarizer with Haiku backend
pub struct WorkspaceSummarizer {
    /// API key for Anthropic
    api_key: Option<String>,
    /// HTTP client
    client: reqwest::Client,
    /// Cache: workspace -> title
    cache: RwLock<HashMap<i32, CachedTitle>>,
    /// Minimum time between summarizations per workspace
    debounce: Duration,
    /// How long cached titles remain valid
    ttl: Duration,
}

impl WorkspaceSummarizer {
    /// Create a new summarizer
    ///
    /// Reads ANTHROPIC_API_KEY from environment. If not set, summarization
    /// will be disabled and fallback titles will be used.
    pub fn new() -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        if api_key.is_none() {
            tracing::info!("ANTHROPIC_API_KEY not set, workspace summarization disabled");
        }

        Self {
            api_key,
            client: reqwest::Client::new(),
            cache: RwLock::new(HashMap::new()),
            debounce: Duration::from_secs(10),
            ttl: Duration::from_secs(300), // 5 minutes
        }
    }

    /// Check if summarization is enabled
    pub fn is_enabled(&self) -> bool {
        self.api_key.is_some()
    }

    /// Summarize sessions on a workspace into an ambient title
    ///
    /// Returns cached title if still valid, otherwise generates new one.
    /// Falls back to generic title on API errors.
    pub async fn summarize(
        &self,
        workspace: i32,
        sessions: Vec<SessionSummaryInput>,
    ) -> Result<String> {
        // Fallback for no sessions
        if sessions.is_empty() {
            return Ok(String::new());
        }

        // Fallback if API key not configured
        let api_key = match &self.api_key {
            Some(k) => k,
            None => {
                tracing::debug!(workspace, "Summarization skipped: ANTHROPIC_API_KEY not set");
                return Ok(self.fallback_title(&sessions));
            }
        };

        // Compute input hash to detect changes
        let input_hash = self.hash_sessions(&sessions);

        // Check cache
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(&workspace) {
                let age = cached.generated_at.elapsed();

                // Return cached if: same input hash OR within debounce period
                if cached.input_hash == input_hash || age < self.debounce {
                    return Ok(cached.title.clone());
                }

                // Return cached if within TTL and input hasn't changed drastically
                if age < self.ttl {
                    return Ok(cached.title.clone());
                }
            }
        }

        // Generate new title
        let title = match self.call_haiku(api_key, &sessions).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "Haiku API summarization failed, using fallback");
                self.fallback_title(&sessions)
            }
        };

        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(workspace, CachedTitle {
                title: title.clone(),
                generated_at: Instant::now(),
                input_hash,
            });
        }

        Ok(title)
    }

    /// Call Haiku API for summarization
    async fn call_haiku(
        &self,
        api_key: &str,
        sessions: &[SessionSummaryInput],
    ) -> Result<String> {
        // Build session descriptions
        let session_descriptions: Vec<String> = sessions
            .iter()
            .map(|s| {
                let mut desc = format!("- Project: {}", s.project_path);
                if let Some(title) = &s.window_title {
                    desc.push_str(&format!("\n  Title: {}", title));
                }
                if let Some(activity) = &s.recent_activity {
                    desc.push_str(&format!("\n  Activity: {}", activity));
                }
                desc
            })
            .collect();

        let prompt = format!(
            r#"You are a workspace title generator. Given the following Claude Code sessions on a single desktop workspace, produce a 2-5 word title that captures the ambient work happening.

Sessions:
{}

Rules:
- Be concise (max 5 words)
- Prefer action verbs ("refactoring X", "debugging Y", "building Z")
- If multiple unrelated sessions, use count ("3 parallel tasks")
- No quotes, no punctuation
- Lowercase

Title:"#,
            session_descriptions.join("\n")
        );

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "claude-3-5-haiku-latest",
                "max_tokens": 32,
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
            .unwrap_or("claude workspace")
            .trim()
            .to_lowercase();

        Ok(title)
    }

    /// Generate fallback title without API call
    fn fallback_title(&self, sessions: &[SessionSummaryInput]) -> String {
        if sessions.len() == 1 {
            // Use project name for single session
            let path = &sessions[0].project_path;
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("claude");
            name.to_string()
        } else {
            // Count for multiple sessions
            format!("{} claude sessions", sessions.len())
        }
    }

    /// Hash sessions for change detection
    fn hash_sessions(&self, sessions: &[SessionSummaryInput]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        for s in sessions {
            s.project_path.hash(&mut hasher);
            s.window_title.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Invalidate cache for a workspace
    pub async fn invalidate(&self, workspace: i32) {
        let mut cache = self.cache.write().await;
        cache.remove(&workspace);
    }

    /// Clear all cached titles
    pub async fn clear_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }
}

impl Default for WorkspaceSummarizer {
    fn default() -> Self {
        Self::new()
    }
}
