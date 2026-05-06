use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Aider,
    slug: "aider",
    display: "Aider",
    accent: "#14B014",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#14B014"),
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["aider"],
    events: events::NONE,
    resume_cmd: None,
};
