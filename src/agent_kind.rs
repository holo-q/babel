//! Harness roster and identity policy.
//!
//! Babel's live truth comes from harness lifecycle hooks plus kitty pane
//! addressing. Scrollback and title fingerprinting are cold-start recovery
//! paths, not the primary identity mechanism. This module keeps the roster in
//! one place so adding a harness is a data update instead of another branch in
//! hook parsing, paint, install generation, and process detection.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Integration tier for a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSupport {
    /// Babel can consume lifecycle hooks directly.
    Supported,
    /// The harness has a plugin/callback surface; it needs a small bridge.
    BridgeRequired,
    /// No stable lifecycle + identity surface. Babel should not guess.
    Unsupported,
}

/// How `babel hook install` should present integration instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStrategy {
    /// Safe local config path known; Babel may write it.
    AutoJsonSettings,
    /// Print JSON hook config; exact target path is harness-owned.
    JsonSnippet,
    /// Print TOML hook config.
    TomlSnippet,
    /// Print filesystem hook layout instructions.
    FilesystemSnippet,
    /// Document the canonical callback payload; provider plugin not implemented.
    BridgeContract,
    /// No install path.
    Unsupported,
}

/// State transition requested by a native harness hook.
///
/// This mirrors Babel's stored hook state without making the roster depend on
/// sqlite/storage details. The hook executor converts it at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStateEffect {
    Working,
    Idle,
    ToolRunning,
}

/// Read/unread action coupled to a hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadEffect {
    #[default]
    Preserve,
    MarkRead,
    MarkUnread,
}

/// Visual pulse requested by a hook.
///
/// Some harnesses lack full state coverage but still emit useful lifecycle
/// edges. The roster can represent those as pulse-only events instead of
/// forcing fake state transitions into the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PulseEffect {
    #[default]
    None,
    Session,
    Prompt,
    Tool,
    Finished,
    Attention,
    Compact,
}

/// One native hook event mapped into Babel's canonical hook vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEventSpec {
    pub native: &'static str,
    pub canonical: &'static str,
    pub state: Option<HookStateEffect>,
    pub read: ReadEffect,
    pub pulse: PulseEffect,
}

/// Static roster entry.
#[derive(Debug, Clone, Copy)]
pub struct HarnessSpec {
    pub kind: AgentKind,
    pub slug: &'static str,
    pub display: &'static str,
    pub accent: &'static str,
    pub support: HarnessSupport,
    pub install: InstallStrategy,
    pub identity_fields: &'static [&'static str],
    pub env_identity_fields: &'static [&'static str],
    pub cmdline_markers: &'static [&'static str],
    pub events: &'static [HookEventSpec],
    /// CLI resume command template. `{}` is replaced with the native session id.
    /// None means no known resume surface.
    pub resume_cmd: Option<&'static str>,
}

impl HarnessSpec {
    /// Format a copy-pasteable resume command for the given native session id.
    pub fn resume_command(&self, native_id: &str) -> Option<String> {
        self.resume_cmd.map(|tpl| tpl.replace("{}", native_id))
    }

    pub fn event(&self, name: &str) -> Option<&'static HookEventSpec> {
        let normalized = name.trim();
        self.events.iter().find(|event| {
            event.native.eq_ignore_ascii_case(normalized)
                || event.canonical.eq_ignore_ascii_case(normalized)
        })
    }
}

/// Which agent harness is running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    FactoryDroid,
    QwenCode,
    Kimi,
    Gemini,
    Crush,
    Cursor,
    Cline,
    OpenCode,
    Amp,
    Kiro,
    GithubCopilot,
    RooCode,
    KiloCode,
    Aider,
    Antigravity,
    /// Unknown / not-an-agent terminal. Carried so terminal events can still
    /// flow through agent-aware code paths without forcing an Option<> at
    /// every call site.
    Other,
}

impl AgentKind {
    /// All known real harnesses, excluding `Other`.
    pub const ALL: &'static [AgentKind] = &[
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::FactoryDroid,
        AgentKind::QwenCode,
        AgentKind::Kimi,
        AgentKind::Gemini,
        AgentKind::Crush,
        AgentKind::Cursor,
        AgentKind::Cline,
        AgentKind::OpenCode,
        AgentKind::Amp,
        AgentKind::Kiro,
        AgentKind::GithubCopilot,
        AgentKind::RooCode,
        AgentKind::KiloCode,
        AgentKind::Aider,
        AgentKind::Antigravity,
    ];

    /// Identify the agent from a foreground process command line.
    pub fn from_cmdline<S: AsRef<str>>(cmdline_args: &[S]) -> Option<Self> {
        for arg in cmdline_args {
            let arg = arg.as_ref().to_ascii_lowercase();
            for kind in Self::ALL {
                if kind
                    .spec()
                    .cmdline_markers
                    .iter()
                    .any(|marker| arg.contains(marker))
                {
                    return Some(*kind);
                }
            }
        }
        None
    }

    pub fn spec(self) -> &'static HarnessSpec {
        crate::harness::spec_for(self)
    }

    /// Lower-case slug used in logs, IPC payloads, session keys.
    pub fn slug(self) -> &'static str {
        self.spec().slug
    }

    pub fn display_name(self) -> &'static str {
        self.spec().display
    }

    /// Canonical harness accent for visual affordances.
    ///
    /// These are domain semantics, not user-configurable theme knobs. The panel
    /// puppets receive pre-baked paint commands from Babel and should not decide
    /// that Codex is cyan just because an old splash screen once was.
    pub fn accent_color(self) -> &'static str {
        self.spec().accent
    }

    pub fn support(self) -> HarnessSupport {
        self.spec().support
    }

    pub fn is_supported(self) -> bool {
        matches!(self.support(), HarnessSupport::Supported)
    }

    /// Convert a native harness session/task/conversation id into Babel's
    /// durable key. Native ids remain provider-local; Babel keys are
    /// namespaced so different harnesses cannot collide.
    pub fn session_key(self, native_id: &str) -> String {
        format!("{}:{}", self.slug(), native_id)
    }

    /// Strip Babel's harness namespace when provider-native storage needs its
    /// original id. If the key is not namespaced, returns it unchanged.
    pub fn native_session_id<'a>(session_key: &'a str) -> &'a str {
        session_key
            .split_once(':')
            .map(|(_, native)| native)
            .unwrap_or(session_key)
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::from_str(s).ok()
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

impl FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        let kind = match normalized.as_str() {
            "claude" | "claude-code" | "cc" => AgentKind::Claude,
            "codex" | "codex-cli" => AgentKind::Codex,
            "factory" | "factory-droid" | "droid" => AgentKind::FactoryDroid,
            "qwen" | "qwen-code" => AgentKind::QwenCode,
            "kimi" | "kimi-cli" => AgentKind::Kimi,
            "gemini" | "gemini-cli" => AgentKind::Gemini,
            "crush" => AgentKind::Crush,
            "cursor" | "cursor-agent" => AgentKind::Cursor,
            "cline" => AgentKind::Cline,
            "opencode" | "open-code" | "sst-opencode" => AgentKind::OpenCode,
            "amp" | "sourcegraph-amp" => AgentKind::Amp,
            "kiro" => AgentKind::Kiro,
            "github-copilot" | "github-copilot-cli" | "copilot" | "copilot-cli" => {
                AgentKind::GithubCopilot
            }
            "roo" | "roo-code" => AgentKind::RooCode,
            "kilo" | "kilo-code" => AgentKind::KiloCode,
            "aider" => AgentKind::Aider,
            "antigravity" => AgentKind::Antigravity,
            "other" | "unknown" => AgentKind::Other,
            _ => return Err(format!("unknown agent kind: {s}")),
        };
        Ok(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn cmdline_detection() {
        assert_eq!(
            AgentKind::from_cmdline(&["/usr/bin/claude", "--print"]),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::from_cmdline(&["codex", "tui"]),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            AgentKind::from_cmdline(&["qwen-code"]),
            Some(AgentKind::QwenCode)
        );
        assert_eq!(
            AgentKind::from_cmdline(&["fish"]),
            None,
            "shell-only panes have no agent"
        );
    }

    #[test]
    fn default_is_agent() {
        // Backwards compat for old serialized fields. New hook keys are still
        // namespaced at ingestion.
        assert_eq!(AgentKind::default(), AgentKind::Claude);
    }

    #[test]
    fn json_roundtrip() {
        let json = serde_json::to_string(&AgentKind::FactoryDroid).unwrap();
        assert_eq!(json, "\"factory-droid\"");
        let parsed: AgentKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AgentKind::FactoryDroid);
    }

    #[test]
    fn roster_slugs_are_unique_and_colored() {
        let mut slugs = HashSet::new();
        for kind in AgentKind::ALL {
            let spec = kind.spec();
            assert!(slugs.insert(spec.slug), "duplicate slug {}", spec.slug);
            assert!(
                spec.accent.starts_with('#'),
                "{} has invalid accent",
                spec.slug
            );
        }
    }

    #[test]
    fn session_keys_are_namespaced() {
        assert_eq!(AgentKind::Claude.session_key("abc"), "claude:abc");
        assert_eq!(AgentKind::Cursor.session_key("conv"), "cursor:conv");
        assert_eq!(AgentKind::native_session_id("cursor:conv"), "conv");
    }

    #[test]
    fn hook_flows_live_in_the_roster() {
        let stop = AgentKind::Claude.spec().event("Stop").unwrap();
        assert_eq!(stop.canonical, "stop");
        assert_eq!(stop.state, Some(HookStateEffect::Idle));
        assert_eq!(stop.read, ReadEffect::MarkUnread);
        assert_eq!(stop.pulse, PulseEffect::Finished);

        let crush = AgentKind::Crush.spec();
        assert!(crush.event("PreToolUse").is_some());
        assert!(
            crush.event("Stop").is_none(),
            "partial harnesses should not synthesize hooks they do not have"
        );
    }
}
