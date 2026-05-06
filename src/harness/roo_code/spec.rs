use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::RooCode,
    slug: "roo-code",
    display: "Roo Code",
    accent: "#D8F14B",
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["roo"],
    events: events::NONE,
    resume_cmd: Some("code"),
};
