use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::QwenCode,
    slug: "qwen-code",
    display: "Qwen Code",
    accent: "#624BEA",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#624BEA"),
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["qwen"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("qwen --resume {}"),
};
