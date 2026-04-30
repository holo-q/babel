use anyhow::Result;

use crate::agent_kind::AgentKind;

use super::{HarnessMigrationReport, HarnessOpsContext};

pub(super) fn plan(
    context: &HarnessOpsContext,
    needles: &[String],
) -> Result<HarnessMigrationReport> {
    super::plan_text_storage_harness(
        context.gemini_tmp(),
        AgentKind::Gemini,
        needles,
        "Gemini project identity is hash/path based; doctor reports references before apply exists.",
    )
}
