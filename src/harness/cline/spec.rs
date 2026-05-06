use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use crate::harness::events;

pub(crate) const SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Cline,
    slug: "cline",
    display: "Cline",
    accent: "#9663F0",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#9663F0"),
    support: HarnessSupport::Supported,
    install: InstallStrategy::FilesystemSnippet,
    identity_fields: &["taskId", "task_id"],
    env_identity_fields: &[],
    cmdline_markers: &["cline"],
    events: events::CLINE,
    resume_cmd: Some("code"),
};
