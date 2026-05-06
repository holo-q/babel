use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Claude,
    slug: "claude",
    display: "Claude Code",
    accent: "#D97757",
    support: HarnessSupport::Supported,
    install: InstallStrategy::AutoJsonSettings,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["claude"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("claude --resume {}"),
};
