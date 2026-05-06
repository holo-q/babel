//! Native harness session discovery.
//!
//! Each harness owns its storage parser behind [`NativeSessionScanner`]. The
//! CLI list/resume surfaces only consume normalized [`NativeSession`] rows.

use anyhow::Result;

use crate::AgentKind;

mod aider;
mod amp;
mod antigravity;
mod claude;
mod cline;
mod codex;
mod crush;
mod cursor;
mod factory_droid;
mod gemini;
mod github_copilot;
mod kilo_code;
mod kimi;
mod kiro;
mod opencode;
mod qwen_code;
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
