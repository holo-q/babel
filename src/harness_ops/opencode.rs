use crate::agent_kind::AgentKind;

use super::{HarnessMigrationReport, HarnessOpsContext};

pub(super) fn plan(context: &HarnessOpsContext) -> HarnessMigrationReport {
    super::plan_roots_only_harness(
        AgentKind::OpenCode,
        context.opencode_roots(),
        "OpenCode uses an in-process plugin model and local database storage; no mutation adapter exists.",
    )
}
