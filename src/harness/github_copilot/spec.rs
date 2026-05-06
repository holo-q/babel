use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::GithubCopilot,
    slug: "github-copilot-cli",
    display: "GitHub Copilot CLI",
    accent: "#8250DF",
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &["copilot"],
    events: events::NONE,
    resume_cmd: Some("copilot --resume {}"),
};
