//! Conversation History Pager
//!
//! Interactive TUI for browsing and launching cross-harness resumes.
//!
//! Access via:
//!   - `babel resume` - Interactive pager
//!   - `babel continue` - Resume most recent non-running session

mod app;
mod jsonl_parser;
mod preferences;
mod project_metrics;
mod session_list;
mod transcript;
mod ui;

pub use app::{launch_harness_resume, run_resume_pager, ResumeApp, ResumeSelection, ResumeSessionSource};
pub use jsonl_parser::parse_transcript;
pub use session_list::{EnrichedSession, RunningStatus};
pub use transcript::TranscriptView;

/// Collapse whitespace runs into a single space -- used by both
/// tool-call preview formatting (ui.rs) and transcript summaries.
pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
