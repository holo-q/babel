use crate::agent_kind::AgentKind;

use super::{AdapterReadiness, HarnessMigrationReport, HarnessOpsContext};

pub(super) fn plan(context: &HarnessOpsContext) -> HarnessMigrationReport {
    let roots: Vec<_> = context
        .cursor_roots()
        .into_iter()
        .filter(|root| root.exists())
        .collect();

    let mut notes = vec![
        "Cursor is reconnaissance-only until workspaceStorage/globalStorage fixtures exist."
            .to_string(),
        "cursor-chat-recovery-kit closes Cursor, backs up current state.vscdb, copies old state.vscdb/images, then restarts Cursor."
            .to_string(),
    ];
    if roots.is_empty() {
        notes.push("no Cursor state roots detected".to_string());
    }

    HarnessMigrationReport::from_edits(
        AgentKind::Cursor,
        AdapterReadiness::ReconOnly,
        roots,
        0,
        0,
        Vec::new(),
        notes,
    )
}
