use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::OpenCode,
    slug: "opencode",
    display: "OpenCode",
    accent: "#FAB283",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#FAB283"),
    support: HarnessSupport::BridgeRequired,
    install: InstallStrategy::BridgeContract,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["opencode", "open-code"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("opencode --session {}"),
};
