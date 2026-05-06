use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::KiloCode,
    slug: "kilo-code",
    display: "Kilo Code",
    accent: "#FA483A",
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["kilo"],
    events: events::NONE,
    resume_cmd: Some("code"),
};
