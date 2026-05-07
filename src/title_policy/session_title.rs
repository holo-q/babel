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

use crate::AgentKind;
use crate::babel_storage::{
    init_db, recent_title_history, record_title_history, set_generated_title,
};
use crate::config::load_config;
use crate::pager::{distilled_human_prompt, prepare_transcript_messages};
use crate::title_policy::RollingPromptsPolicy;

#[derive(Debug, Clone)]
pub struct SessionTitleTarget {
    pub agent_kind: AgentKind,
    pub native_id: String,
    pub session_key: String,
    pub project_path: Option<PathBuf>,
    pub native_title: Option<String>,
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
    let prompts = read_distilled_user_prompts(target.agent_kind, &target.native_id).await?;
    if prompts.is_empty() {
        anyhow::bail!("no user prompts found for {}", target.session_key);
    }

    let conn = init_db().context("open babel overlay DB")?;
    if let Some(native_title) = target.native_title.as_deref() {
        record_title_history(&conn, &target.session_key, native_title, "native")?;
    }
    let prior_titles = recent_title_history(
        &conn,
        &target.session_key,
        config.title_policy.rolling_prompts.title_history_count,
    )?;

    let policy = RollingPromptsPolicy::new(config.title_policy.rolling_prompts.clone());
    let title = policy
        .generate_from_prompts_with_titles(&prompts, &prior_titles)
        .await?
        .context("title generation unavailable; check ANTHROPIC_API_KEY")?;

    let settle_delay = Duration::from_millis(config.title_policy.storage.jsonl_settle_delay_ms);
    harness
        .write_title(&target.native_id, &title, settle_delay)
        .await?;

    set_generated_title(&conn, &target.session_key, &title)?;

    Ok(SessionTitleUpdate {
        session_key: target.session_key,
        title,
    })
}

async fn read_distilled_user_prompts(kind: AgentKind, native_id: &str) -> Result<Vec<String>> {
    let Some(path) = crate::harness::find_session_transcript(kind, native_id)
        .with_context(|| format!("find transcript for {native_id}"))?
    else {
        anyhow::bail!("transcript not found for {native_id}");
    };

    let mut messages = crate::harness::parse_transcript(kind, &path)
        .with_context(|| format!("parse transcript {}", path.display()))?;
    prepare_transcript_messages(&mut messages);

    Ok(messages
        .iter()
        .filter(|message| matches!(message.kind, scrollparse::MessageKind::User))
        .filter_map(|message| distilled_human_prompt(&message.content))
        .collect())
}

fn title_harness(kind: AgentKind) -> Result<Box<dyn SessionTitleHarness>> {
    match kind {
        AgentKind::Claude => Ok(Box::new(crate::harness::claude::title::TitleHarness)),
        AgentKind::Codex => Ok(Box::new(crate::harness::codex::title::TitleHarness)),
        _ => anyhow::bail!("{} title writer is not wired yet", kind.display_name()),
    }
}
