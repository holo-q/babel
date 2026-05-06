use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::QwenCode,
    slug: "qwen-code",
    display: "Qwen Code",
    accent: "#624BEA",
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["qwen"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("qwen --resume {}"),
};
