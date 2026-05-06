use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Cursor,
    slug: "cursor",
    display: "Cursor Agent",
    accent: "#14120B",
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["conversation_id"],
    env_identity_fields: &[],
    cmdline_markers: &["cursor-agent", "cursor"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("cursor-agent --resume {}"),
};
