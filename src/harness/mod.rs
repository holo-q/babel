//! Harness-owned protocol registry.
//!
//! Feature entrypoints such as `native_sessions` and `harness_ops` stay
//! domain-shaped, but each harness owns its static protocol facts here beside
//! its session scanner and migration adapter.

use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};

mod events;

#[path = "aider/spec.rs"]
mod aider;
#[path = "amp/spec.rs"]
mod amp;
#[path = "antigravity/spec.rs"]
mod antigravity;
#[path = "claude/spec.rs"]
mod claude;
#[path = "cline/spec.rs"]
mod cline;
pub(crate) mod codex;
#[path = "crush/spec.rs"]
mod crush;
#[path = "cursor/spec.rs"]
mod cursor;
#[path = "factory_droid/spec.rs"]
mod factory_droid;
#[path = "gemini/spec.rs"]
mod gemini;
#[path = "github_copilot/spec.rs"]
mod github_copilot;
#[path = "kilo_code/spec.rs"]
mod kilo_code;
#[path = "kimi/spec.rs"]
mod kimi;
#[path = "kiro/spec.rs"]
mod kiro;
#[path = "opencode/spec.rs"]
mod opencode;
#[path = "qwen_code/spec.rs"]
mod qwen_code;
#[path = "roo_code/spec.rs"]
mod roo_code;

pub const HARNESS_SPECS: &[HarnessSpec] = &[
    claude::SPEC,
    codex::spec::SPEC,
    factory_droid::SPEC,
    qwen_code::SPEC,
    kimi::SPEC,
    gemini::SPEC,
    crush::SPEC,
    cursor::SPEC,
    cline::SPEC,
    opencode::SPEC,
    amp::SPEC,
    kiro::SPEC,
    github_copilot::SPEC,
    roo_code::SPEC,
    kilo_code::SPEC,
    aider::SPEC,
    antigravity::SPEC,
];

const OTHER_SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Other,
    slug: "other",
    display: "Other",
    accent: "#666666",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#666666"),
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &[],
    resume_cmd: None,
    events: events::NONE,
};

pub fn spec_for(kind: AgentKind) -> &'static HarnessSpec {
    HARNESS_SPECS
        .iter()
        .find(|spec| spec.kind == kind)
        .unwrap_or(&OTHER_SPEC)
}
