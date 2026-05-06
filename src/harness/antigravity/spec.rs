use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Antigravity,
    slug: "antigravity",
    display: "Antigravity",
    accent: "#3186FF",
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["antigravity"],
    events: events::NONE,
    resume_cmd: Some("antigravity"),
};
