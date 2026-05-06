//! Native harness session discovery.
//!
//! Each harness owns its storage parser behind [`NativeSessionScanner`]. The
//! CLI list/resume surfaces only consume normalized [`NativeSession`] rows.

use anyhow::Result;

use crate::AgentKind;

// Harness protocols live together on disk under `src/harness/<kind>/`.
// This registry only exposes the read-only native session discovery facet.
#[path = "../harness/aider/sessions.rs"]
mod aider;
#[path = "../harness/amp/sessions.rs"]
mod amp;
#[path = "../harness/antigravity/sessions.rs"]
mod antigravity;
#[path = "../harness/claude/sessions.rs"]
mod claude;
#[path = "../harness/cline/sessions.rs"]
mod cline;
#[path = "../harness/codex/sessions.rs"]
mod codex;
#[path = "../harness/crush/sessions.rs"]
mod crush;
#[path = "../harness/cursor/sessions.rs"]
mod cursor;
#[path = "../harness/factory_droid/sessions.rs"]
mod factory_droid;
#[path = "../harness/gemini/sessions.rs"]
mod gemini;
#[path = "../harness/github_copilot/sessions.rs"]
mod github_copilot;
#[path = "../harness/kilo_code/sessions.rs"]
mod kilo_code;
#[path = "../harness/kimi/sessions.rs"]
mod kimi;
#[path = "../harness/kiro/sessions.rs"]
mod kiro;
#[path = "../harness/opencode/sessions.rs"]
mod opencode;
#[path = "../harness/qwen_code/sessions.rs"]
mod qwen_code;
#[path = "../harness/roo_code/sessions.rs"]
mod roo_code;
mod shared;

pub use shared::{NativeSession, SessionFilters};

pub trait NativeSessionScanner: Send + Sync {
    fn agent_kind(&self) -> AgentKind;
    fn scan(&self) -> Result<Vec<NativeSession>>;
}

static AIDER: aider::AiderScanner = aider::AiderScanner;
static AMP: amp::AmpScanner = amp::AmpScanner;
static ANTIGRAVITY: antigravity::AntigravityScanner = antigravity::AntigravityScanner;
static CLAUDE: claude::ClaudeScanner = claude::ClaudeScanner;
static CLINE: cline::ClineScanner = cline::ClineScanner;
static CODEX: codex::CodexScanner = codex::CodexScanner;
static CRUSH: crush::CrushScanner = crush::CrushScanner;
static CURSOR: cursor::CursorScanner = cursor::CursorScanner;
static FACTORY_DROID: factory_droid::FactoryDroidScanner = factory_droid::FactoryDroidScanner;
static GEMINI: gemini::GeminiScanner = gemini::GeminiScanner;
static GITHUB_COPILOT: github_copilot::GithubCopilotScanner = github_copilot::GithubCopilotScanner;
static KILO_CODE: kilo_code::KiloCodeScanner = kilo_code::KiloCodeScanner;
static KIMI: kimi::KimiScanner = kimi::KimiScanner;
static KIRO: kiro::KiroScanner = kiro::KiroScanner;
static OPENCODE: opencode::OpenCodeScanner = opencode::OpenCodeScanner;
static QWEN_CODE: qwen_code::QwenCodeScanner = qwen_code::QwenCodeScanner;
static ROO_CODE: roo_code::RooCodeScanner = roo_code::RooCodeScanner;

fn scanner_for(kind: AgentKind) -> Option<&'static dyn NativeSessionScanner> {
    let scanner: &'static dyn NativeSessionScanner = match kind {
        AgentKind::Claude => &CLAUDE,
        AgentKind::Codex => &CODEX,
        AgentKind::FactoryDroid => &FACTORY_DROID,
        AgentKind::QwenCode => &QWEN_CODE,
        AgentKind::Kimi => &KIMI,
        AgentKind::Gemini => &GEMINI,
        AgentKind::Crush => &CRUSH,
        AgentKind::Cursor => &CURSOR,
        AgentKind::Cline => &CLINE,
        AgentKind::OpenCode => &OPENCODE,
        AgentKind::Amp => &AMP,
        AgentKind::Kiro => &KIRO,
        AgentKind::GithubCopilot => &GITHUB_COPILOT,
        AgentKind::RooCode => &ROO_CODE,
        AgentKind::KiloCode => &KILO_CODE,
        AgentKind::Aider => &AIDER,
        AgentKind::Antigravity => &ANTIGRAVITY,
        AgentKind::Other => return None,
    };
    Some(scanner)
}

/// Scan all selected harness stores and return sessions sorted by recency.
///
/// Transcript preview intentionally lives behind a separate future trait. This
/// layer only owns listing metadata: ids, cwd hints, titles, prompt snippets,
/// counts, and timestamps.
pub fn scan_all(kind: Option<&str>, filters: &SessionFilters) -> Vec<NativeSession> {
    let kind_filter = kind.and_then(AgentKind::from_slug);
    let kinds: Vec<AgentKind> = kind_filter
        .map(|kind| vec![kind])
        .unwrap_or_else(|| AgentKind::ALL.to_vec());

    let mut sessions = Vec::new();
    for kind in kinds {
        let Some(scanner) = scanner_for(kind) else {
            continue;
        };
        sessions.extend(scanner.scan().unwrap_or_default());
    }

    sessions.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
    sessions.retain(|s| {
        !s.last_prompt.as_deref().is_some_and(|p| {
            let t = p.trim();
            t.starts_with("/end") || t.starts_with(".end")
        })
    });
    if !filters.all {
        if !filters.sub {
            sessions.retain(|s| s.interactive);
        }
        if !filters.oneshot {
            sessions.retain(|s| s.turn_count != 1);
        }
        if !filters.commands {
            sessions.retain(|s| !s.command_only);
        }
    }
    sessions
}
