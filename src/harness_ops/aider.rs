use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, MigrationEdit};

pub(super) fn plan() -> HarnessMigrationReport {
    HarnessMigrationReport::from_edits(
        AgentKind::Aider,
        AdapterReadiness::DoctorOnly,
        Vec::new(),
        0,
        0,
        vec![MigrationEdit::preserve_project_local_history(
            AgentKind::Aider,
            "source directory contents",
            "project-local chat/history files should move with the project itself",
        )],
        vec![
            "Aider is mostly a filesystem move problem; no global session rewrite adapter is expected for v1."
                .to_string(),
        ],
    )
}
