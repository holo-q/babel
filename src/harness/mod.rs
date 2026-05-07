//! Harness-owned protocol registry.
//!
//! Feature entrypoints such as `native_sessions` and `harness_ops` stay
//! domain-shaped, but each harness owns its static protocol facts here beside
//! its session scanner and migration adapter.

use crate::agent_kind::{AgentKind, HarnessSpec, HarnessSupport, InstallStrategy};
use std::path::Path;

mod events;

#[path = "aider/spec.rs"]
mod aider;
#[path = "amp/spec.rs"]
mod amp;
#[path = "antigravity/spec.rs"]
mod antigravity;
pub(crate) mod claude;
#[path = "cline/spec.rs"]
mod cline;
pub(crate) mod codex;
#[path = "crush/spec.rs"]
mod crush;
#[path = "cursor/spec.rs"]
mod cursor;
#[path = "factory_droid/spec.rs"]
mod factory_droid;
#[path = "gemini/spec.rs"]
mod gemini;
#[path = "github_copilot/spec.rs"]
mod github_copilot;
#[path = "kilo_code/spec.rs"]
mod kilo_code;
#[path = "kimi/spec.rs"]
mod kimi;
#[path = "kiro/spec.rs"]
mod kiro;
#[path = "opencode/spec.rs"]
mod opencode;
#[path = "qwen_code/spec.rs"]
mod qwen_code;
#[path = "roo_code/spec.rs"]
mod roo_code;

pub const HARNESS_SPECS: &[HarnessSpec] = &[
    claude::spec::SPEC,
    codex::spec::SPEC,
    factory_droid::SPEC,
    qwen_code::SPEC,
    kimi::SPEC,
    gemini::SPEC,
    crush::SPEC,
    cursor::SPEC,
    cline::SPEC,
    opencode::SPEC,
    amp::SPEC,
    kiro::SPEC,
    github_copilot::SPEC,
    roo_code::SPEC,
    kilo_code::SPEC,
    aider::SPEC,
    antigravity::SPEC,
];

const OTHER_SPEC: HarnessSpec = HarnessSpec {
    kind: AgentKind::Other,
    slug: "other",
    display: "Other",
    accent: "#666666",
    ansi256: crate::agent_kind::accent_to_ansi256(b"#666666"),
    support: HarnessSupport::Unsupported,
    install: InstallStrategy::Unsupported,
    identity_fields: &[],
    env_identity_fields: &[],
    cmdline_markers: &[],
    resume_cmd: None,
    events: events::NONE,
};

pub fn spec_for(kind: AgentKind) -> &'static HarnessSpec {
    HARNESS_SPECS
        .iter()
        .find(|spec| spec.kind == kind)
        .unwrap_or(&OTHER_SPEC)
}

/// Build native argv for resuming a harness session.
///
/// Harness-specific command details live behind each harness module. The pager
/// and CLI launch surfaces should not know that Codex needs `--cd`, Claude uses
/// `--resume`, or a future harness needs environment/bootstrap flags.
pub fn resume_command_parts(
    kind: AgentKind,
    native_id: &str,
    cwd: Option<&Path>,
) -> Option<Vec<String>> {
    match kind {
        AgentKind::Codex => Some(codex::resume_command_parts(native_id, cwd)),
        _ => spec_for(kind)
            .resume_command(native_id)
            .map(split_static_resume_command),
    }
}

pub fn resume_command_display(
    kind: AgentKind,
    native_id: &str,
    cwd: Option<&Path>,
) -> Option<String> {
    resume_command_parts(kind, native_id, cwd).map(|parts| {
        parts
            .iter()
            .map(|part| shell_quote_for_display(part))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn split_static_resume_command(command: String) -> Vec<String> {
    command.split_whitespace().map(str::to_string).collect()
}

fn shell_quote_for_display(arg: &str) -> String {
    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }

    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_resume_passes_session_cwd_before_session_id() {
        let parts = resume_command_parts(
            AgentKind::Codex,
            "019dd59f-ca6b-7100-885b-8aca07b12b15",
            Some(Path::new("/home/nuck/holoq/repo-os/babel")),
        )
        .unwrap();

        assert_eq!(
            parts,
            [
                "codex",
                "resume",
                "--cd",
                "/home/nuck/holoq/repo-os/babel",
                "019dd59f-ca6b-7100-885b-8aca07b12b15",
            ]
        );
    }

    #[test]
    fn resume_command_display_quotes_cwd_with_spaces() {
        let command = resume_command_display(
            AgentKind::Codex,
            "session",
            Some(Path::new("/workspace/has space")),
        )
        .unwrap();

        assert_eq!(command, "codex resume --cd '/workspace/has space' session");
    }
}

/// Locate the transcript file for a session by harness-specific storage convention.
pub fn find_session_transcript(
    kind: AgentKind,
    native_id: &str,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    match kind {
        AgentKind::Claude => crate::utility::claude_storage::find_session_transcript(native_id),
        AgentKind::Codex => codex::transcript::find_session_transcript(native_id),
        _ => Ok(None),
    }
}

/// Parse a transcript file using the harness-specific format.
pub fn parse_transcript(
    kind: AgentKind,
    path: &std::path::Path,
) -> anyhow::Result<Vec<scrollparse::Message>> {
    match kind {
        AgentKind::Claude => claude::transcript::parse_transcript(path),
        AgentKind::Codex => codex::transcript::parse_transcript(path),
        _ => anyhow::bail!(
            "{} transcript parsing is not wired yet",
            kind.display_name()
        ),
    }
}
