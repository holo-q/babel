use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Codex,
    slug: "codex",
    display: "Codex CLI",
    accent: "#10A37F",
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["codex"],
    events: events::CODEX,
    resume_cmd: Some("codex resume {}"),
};
