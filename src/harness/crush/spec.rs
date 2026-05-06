use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Crush,
    slug: "crush",
    display: "Crush",
    accent: "#6B50FF",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#6B50FF"),
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &["CRUSH_SESSION_ID"],
    cmdline_markers: &["crush"],
    events: events::CRUSH,
    resume_cmd: Some("crush --session {}"),
};
