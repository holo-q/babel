use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{HarnessMigrationReport, HarnessOpsContext};

pub(super) fn plan(
    context: &HarnessOpsContext,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    super::plan_text_storage_harness(
        context.qwen_base(),
        AgentKind::QwenCode,
        needles,
        "Qwen has compatible hook identity, but path-move storage rewrite still needs native fixtures.",
    )
}
