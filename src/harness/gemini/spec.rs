use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Gemini,
    slug: "gemini",
    display: "Gemini CLI",
    accent: "#3186FF",
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &["GEMINI_SESSION_ID"],
    cmdline_markers: &["gemini"],
    events: events::GEMINI,
    resume_cmd: Some("gemini --resume"),
};
