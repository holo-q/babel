//! Persisted resume TUI display preferences.
//!
//! These are user-level view defaults, not session metadata: toggles should
//! survive closing the launcher, but they should not live in the overlay DB
//! beside per-conversation facts.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::session_list::{CwdDisplayMode, HiddenDisplayMode, SortColumn, SortDirection};
use super::transcript::{TranscriptBodyMode, TranscriptRoleFilter};

const RESUME_DISPLAY_PREFS: &str = "resume-display.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeDisplayOptions {
    pub show_all: bool,
    pub hidden_display_mode: HiddenDisplayMode,
    pub cwd_display_mode: CwdDisplayMode,
    pub sort_column: SortColumn,
    pub sort_direction: SortDirection,
    pub snip_columns: bool,
    pub braille_tokens: bool,
    pub show_transcript: bool,
    pub transcript_body_mode: TranscriptBodyMode,
    /// Legacy persisted field kept only to migrate older preference files.
    #[serde(skip_serializing)]
    pub expand_messages: bool,
    pub transcript_role_filter: TranscriptRoleFilter,
}

impl Default for ResumeDisplayOptions {
    fn default() -> Self {
        Self {
            show_all: false,
            hidden_display_mode: HiddenDisplayMode::Normal,
            cwd_display_mode: CwdDisplayMode::Relative,
            sort_column: SortColumn::ModifiedTime,
            sort_direction: SortDirection::Descending,
            snip_columns: true,
            braille_tokens: false,
            show_transcript: true,
            transcript_body_mode: TranscriptBodyMode::Snip,
            expand_messages: false,
            transcript_role_filter: TranscriptRoleFilter::All,
        }
    }
}

pub fn load_resume_display_options() -> ResumeDisplayOptions {
    load_resume_display_options_from(&resume_display_options_path()).unwrap_or_default()
}

pub fn save_resume_display_options(options: &ResumeDisplayOptions) -> Result<()> {
    save_resume_display_options_to(&resume_display_options_path(), options)
}

fn resume_display_options_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("babel")
        .join(RESUME_DISPLAY_PREFS)
}

fn load_resume_display_options_from(path: &Path) -> Result<ResumeDisplayOptions> {
    if !path.exists() {
        return Ok(ResumeDisplayOptions::default());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read resume display options from {}", path.display()))?;
    let mut options: ResumeDisplayOptions = serde_json::from_str(&text)
        .with_context(|| format!("parse resume display options from {}", path.display()))?;
    if options.transcript_body_mode == TranscriptBodyMode::Snip && options.expand_messages {
        options.transcript_body_mode = TranscriptBodyMode::Full;
    }
    Ok(options)
}

fn save_resume_display_options_to(path: &Path, options: &ResumeDisplayOptions) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create resume display options dir {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(options).context("serialize resume display options")?;
    fs::write(path, text)
        .with_context(|| format!("write resume display options to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_options_roundtrip_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume-display.json");
        let options = ResumeDisplayOptions {
            show_all: true,
            hidden_display_mode: HiddenDisplayMode::All,
            cwd_display_mode: CwdDisplayMode::TouchedProjects,
            sort_column: SortColumn::Thread,
            sort_direction: SortDirection::Ascending,
            snip_columns: false,
            braille_tokens: true,
            show_transcript: false,
            transcript_body_mode: TranscriptBodyMode::Thoughtstream,
            expand_messages: false,
            transcript_role_filter: TranscriptRoleFilter::UserOnly,
        };

        save_resume_display_options_to(&path, &options).unwrap();

        assert_eq!(load_resume_display_options_from(&path).unwrap(), options);
    }

    #[test]
    fn display_options_missing_file_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        assert_eq!(
            load_resume_display_options_from(&path).unwrap(),
            ResumeDisplayOptions::default()
        );
    }
}
