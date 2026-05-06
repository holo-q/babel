use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Amp,
    slug: "amp",
    display: "Amp",
    accent: "#F34E3F",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#F34E3F"),
    support: HarnessSupport::BridgeRequired,
    install: InstallStrategy::BridgeContract,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["amp"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("amp --thread {}"),
};
