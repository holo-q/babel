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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadEffect {
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
}

impl HarnessSpec {
    pub fn event(&self, name: &str) -> Option<&'static HookEventSpec> {
        let normalized = name.trim();
        self.events.iter().find(|event| {
            event.native.eq_ignore_ascii_case(normalized)
                || event.canonical.eq_ignore_ascii_case(normalized)
        })
    }
}

const CC_EVENTS: &[HookEventSpec] = &[
    HookEventSpec {
        native: "SessionStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "Notification",
        canonical: "notification",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Attention,
    },
    HookEventSpec {
        native: "SubagentStop",
        canonical: "subagent-stop",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PreCompact",
        canonical: "pre-compact",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Compact,
    },
];

const CODEX_EVENTS: &[HookEventSpec] = &[
    HookEventSpec {
        native: "SessionStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PermissionRequest",
        canonical: "notification",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Attention,
    },
];

const GEMINI_EVENTS: &[HookEventSpec] = &[
    HookEventSpec {
        native: "BeforeTool",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "AfterTool",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "BeforeAgent",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "Stop",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
    HookEventSpec {
        native: "PreCompress",
        canonical: "pre-compact",
        state: None,
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Compact,
    },
];

const CRUSH_EVENTS: &[HookEventSpec] = &[HookEventSpec {
    native: "PreToolUse",
    canonical: "pre-tool",
    state: Some(HookStateEffect::ToolRunning),
    read: ReadEffect::Preserve,
    pulse: PulseEffect::Tool,
}];

const CLINE_EVENTS: &[HookEventSpec] = &[
    HookEventSpec {
        native: "TaskStart",
        canonical: "session-start",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Session,
    },
    HookEventSpec {
        native: "UserPromptSubmit",
        canonical: "prompt",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::MarkRead,
        pulse: PulseEffect::Prompt,
    },
    HookEventSpec {
        native: "PreToolUse",
        canonical: "pre-tool",
        state: Some(HookStateEffect::ToolRunning),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "PostToolUse",
        canonical: "post-tool",
        state: Some(HookStateEffect::Working),
        read: ReadEffect::Preserve,
        pulse: PulseEffect::Tool,
    },
    HookEventSpec {
        native: "TaskComplete",
        canonical: "stop",
        state: Some(HookStateEffect::Idle),
        read: ReadEffect::MarkUnread,
        pulse: PulseEffect::Finished,
    },
];

const NO_EVENTS: &[HookEventSpec] = &[];

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
        HARNESS_SPECS
            .iter()
            .find(|spec| spec.kind == self)
            .unwrap_or(&OTHER_SPEC)
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

pub const HARNESS_SPECS: &[HarnessSpec] = &[
    HarnessSpec {
        kind: AgentKind::Claude,
        slug: "claude",
        display: "Claude Code",
        accent: "#D97757",
        support: HarnessSupport::Supported,
        install: InstallStrategy::AutoJsonSettings,
        identity_fields: &["session_id"],
        env_identity_fields: &[],
        cmdline_markers: &["claude"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Codex,
        slug: "codex",
        display: "Codex CLI",
        accent: "#10A37F",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &[],
        cmdline_markers: &["codex"],
        events: CODEX_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::FactoryDroid,
        slug: "factory-droid",
        display: "Factory Droid",
        accent: "#D15010",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &[],
        cmdline_markers: &["factory", "droid"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::QwenCode,
        slug: "qwen-code",
        display: "Qwen Code",
        accent: "#624BEA",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &[],
        cmdline_markers: &["qwen"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Kimi,
        slug: "kimi",
        display: "Kimi CLI",
        accent: "#7F1C10",
        support: HarnessSupport::Supported,
        install: InstallStrategy::TomlSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &[],
        cmdline_markers: &["kimi"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Gemini,
        slug: "gemini",
        display: "Gemini CLI",
        accent: "#4285F4",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &["GEMINI_SESSION_ID"],
        cmdline_markers: &["gemini"],
        events: GEMINI_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Crush,
        slug: "crush",
        display: "Crush",
        accent: "#6B50FF",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["session_id"],
        env_identity_fields: &["CRUSH_SESSION_ID"],
        cmdline_markers: &["crush"],
        events: CRUSH_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Cursor,
        slug: "cursor",
        display: "Cursor Agent",
        accent: "#14120B",
        support: HarnessSupport::Supported,
        install: InstallStrategy::JsonSnippet,
        identity_fields: &["conversation_id"],
        env_identity_fields: &[],
        cmdline_markers: &["cursor-agent", "cursor"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Cline,
        slug: "cline",
        display: "Cline",
        accent: "#9663F0",
        support: HarnessSupport::Supported,
        install: InstallStrategy::FilesystemSnippet,
        identity_fields: &["taskId", "task_id"],
        env_identity_fields: &[],
        cmdline_markers: &["cline"],
        events: CLINE_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::OpenCode,
        slug: "opencode",
        display: "OpenCode",
        accent: "#FAB283",
        support: HarnessSupport::BridgeRequired,
        install: InstallStrategy::BridgeContract,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["opencode", "open-code"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Amp,
        slug: "amp",
        display: "Amp",
        accent: "#F34E3F",
        support: HarnessSupport::BridgeRequired,
        install: InstallStrategy::BridgeContract,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["amp"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Kiro,
        slug: "kiro",
        display: "Kiro",
        accent: "#C6A0FF",
        support: HarnessSupport::BridgeRequired,
        install: InstallStrategy::BridgeContract,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["kiro"],
        events: CC_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::GithubCopilot,
        slug: "github-copilot-cli",
        display: "GitHub Copilot CLI",
        accent: "#8250DF",
        support: HarnessSupport::Unsupported,
        install: InstallStrategy::Unsupported,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["copilot"],
        events: NO_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::RooCode,
        slug: "roo-code",
        display: "Roo Code",
        accent: "#D8F14B",
        support: HarnessSupport::Unsupported,
        install: InstallStrategy::Unsupported,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["roo"],
        events: NO_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::KiloCode,
        slug: "kilo-code",
        display: "Kilo Code",
        accent: "#FA483A",
        support: HarnessSupport::Unsupported,
        install: InstallStrategy::Unsupported,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["kilo"],
        events: NO_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Aider,
        slug: "aider",
        display: "Aider",
        accent: "#14B014",
        support: HarnessSupport::Unsupported,
        install: InstallStrategy::Unsupported,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["aider"],
        events: NO_EVENTS,
    },
    HarnessSpec {
        kind: AgentKind::Antigravity,
        slug: "antigravity",
        display: "Antigravity",
        accent: "#3186FF",
        support: HarnessSupport::Unsupported,
        install: InstallStrategy::Unsupported,
        identity_fields: &[],
        env_identity_fields: &[],
        cmdline_markers: &["antigravity"],
        events: NO_EVENTS,
    },
];

const OTHER_SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Other,
    slug: "other",
    display: "Other",
    accent: "#666666",
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &[],
    events: NO_EVENTS,
};

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
