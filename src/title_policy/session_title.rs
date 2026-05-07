//! Manual session title refresh.
//!
//! `babel resume` can rename old conversations from the session list. That is
//! deliberately stricter than ambient workspace naming: a session title is disk
//! identity, so Babel first writes the harness-native title record and only then
//! updates its overlay title cache.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::babel_storage::{init_db, set_generated_title};
use crate::config::load_config;
use crate::title_policy::RollingPromptsPolicy;
use crate::AgentKind;

#[derive(Debug, Clone)]
pub struct SessionTitleTarget {
    pub agent_kind: AgentKind,
    pub native_id: String,
    pub session_key: String,
    pub project_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SessionTitleUpdate {
    pub session_key: String,
    pub title: String,
}

#[async_trait]
pub(crate) trait SessionTitleHarness: Send + Sync {
    async fn read_user_prompts(&self, native_id: &str) -> Result<Vec<String>>;

    async fn write_title(&self, native_id: &str, title: &str, settle_delay: Duration)
        -> Result<()>;
}

/// Generate and persist a title for one stopped native session.
pub async fn generate_session_title(target: SessionTitleTarget) -> Result<SessionTitleUpdate> {
    let config = load_config()?;
    if !config.title_policy.enabled {
        anyhow::bail!("title policy disabled in babel config");
    }
    if config.title_policy.policy != "rolling_prompts" {
        anyhow::bail!("unsupported title policy: {}", config.title_policy.policy);
    }

    let harness = title_harness(target.agent_kind)?;
    let prompts = harness.read_user_prompts(&target.native_id).await?;
    if prompts.is_empty() {
        anyhow::bail!("no user prompts found for {}", target.session_key);
    }

    let policy = RollingPromptsPolicy::new(config.title_policy.rolling_prompts.clone());
    let title = policy
        .generate_from_prompts(&prompts)
        .await?
        .context("title generation unavailable; check ANTHROPIC_API_KEY")?;

    let settle_delay = Duration::from_millis(config.title_policy.storage.jsonl_settle_delay_ms);
    harness
        .write_title(&target.native_id, &title, settle_delay)
        .await?;

    let conn = init_db().context("open babel overlay DB")?;
    set_generated_title(&conn, &target.session_key, &title)?;

    Ok(SessionTitleUpdate {
        session_key: target.session_key,
        title,
    })
}

fn title_harness(kind: AgentKind) -> Result<Box<dyn SessionTitleHarness>> {
    match kind {
        AgentKind::Claude => Ok(Box::new(crate::harness::claude::title::TitleHarness)),
        AgentKind::Codex => Ok(Box::new(crate::harness::codex::title::TitleHarness)),
        _ => anyhow::bail!("{} title writer is not wired yet", kind.display_name()),
    }
}
