use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Kimi,
    slug: "kimi",
    display: "Kimi CLI",
    accent: "#7F1C10",
    support: HarnessSupport::Supported,
    install: InstallStrategy::TomlSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["kimi"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("kimi --session {}"),
};
