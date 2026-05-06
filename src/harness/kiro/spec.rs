use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Kiro,
    slug: "kiro",
    display: "Kiro",
    accent: "#C6A0FF",
    support: HarnessSupport::BridgeRequired,
    install: InstallStrategy::BridgeContract,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["kiro"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("kiro"),
};
