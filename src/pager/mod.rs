//! Conversation History Pager
//!
//! Interactive TUI for browsing and resuming cross-harness sessions.
//!
//! Access via:
//!   - `babel resume` - Interactive pager
//!   - `babel continue` - Resume most recent non-running session

mod app;
mod jsonl_parser;
mod session_list;
mod transcript;
mod ui;

pub use app::{run_resume_pager, ResumeApp, ResumeSelection};
pub use jsonl_parser::parse_transcript;
pub use session_list::{EnrichedSession, RunningStatus};
pub use transcript::TranscriptView;
