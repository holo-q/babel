use crate::agent_kind::AgentKind;

use super::{HarnessMigrationReport, HarnessOpsContext};

pub(super) fn plan(
    context: &HarnessOpsContext,
    harness: AgentKind,
    note: &str,
) -> HarnessMigrationReport {
    super::plan_roots_only_harness(harness, context.vscode_roots(), note)
}
