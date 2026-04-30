use std::path::Path;

use anyhow::Result;

use super::{HarnessMigrationReport, HarnessOpsContext, MigrationRisk};

pub(super) fn plan(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
    risks: &mut Vec<MigrationRisk>,
) -> Result<HarnessMigrationReport> {
    super::plan_claude(context, old_path, new_path, needles, risks)
}
