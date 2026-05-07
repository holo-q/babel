//! Conversation History Pager
//!
//! Interactive TUI for browsing and launching cross-harness resumes.
//!
//! Access via:
//!   - `babel resume` - Interactive pager
//!   - `babel continue` - Resume most recent non-running session

mod app;
mod demo;
mod identity;
mod preferences;
mod project_metrics;
mod session_list;
mod transcript;
mod ui;

pub use crate::harness::claude::transcript::parse_transcript;
pub use app::{
    launch_harness_resume, run_resume_pager, ResumeApp, ResumeSelection, ResumeSessionSource,
};
pub use demo::DemoMode;
pub use project_metrics::{
    load_cached_session_projects, load_session_projects_from_cache, ProjectTouchMetric,
};
pub use session_list::{EnrichedSession, RunningStatus};
pub use transcript::{
    distill_prompt_thoughtstream, distilled_human_prompt, TranscriptBodyMode, TranscriptRoleFilter,
    TranscriptView, TRANSCRIPT_SNIP_MARKER,
};
pub use ui::{
    prepare_transcript_messages, transcript_palette, transcript_visible_lines, TranscriptPalette,
};

/// Collapse whitespace runs into a single space -- used by both
/// tool-call preview formatting (ui.rs) and transcript summaries.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
