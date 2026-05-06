use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::FactoryDroid,
    slug: "factory-droid",
    display: "Factory Droid",
    accent: "#D15010",
    support: HarnessSupport::Supported,
    install: InstallStrategy::JsonSnippet,
    identity_fields: &["session_id"],
    env_identity_fields: &[],
    cmdline_markers: &["factory", "droid"],
    events: events::CLAUDE_CODE,
    resume_cmd: Some("droid --resume {}"),
};
