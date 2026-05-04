//! Harness-aware operations planning.
//!
//! This is the read side of the "consume the boards" plan: Babel can inspect
//! provider-native state and produce an operation graph without making a global
//! search index the source of truth. Native harness storage remains authoritative;
//! any future cache must be rebuildable from these adapters.

use std::path::Path;

use anyhow::Result;

use crate::core::ConflictingPane;

mod aider;
mod amp;
mod antigravity;
mod apply;
mod claude;
mod cline;
mod codex;
mod context;
mod crush;
mod cursor;
mod factory;
mod gemini;
mod github_copilot;
mod kilo;
mod kimi;
mod kiro;
mod opencode;
mod planner;
mod probes;
mod qwen;
mod roo;
mod types;

pub use apply::{apply_migration_plan, MigrationApplyOptions, MigrationApplyReport};
pub use apply::{
    migration_manifest_root, migration_manifests_by_ref, recent_migration_manifests,
    MigrationManifestEntry,
};
pub use context::HarnessOpsContext;
pub use planner::supported_operation_harnesses;
pub use types::{
    AdapterReadiness, ApplyCapability, HarnessMigrationReport, LivePaneImpact,
    MigrationDoctorReport, MigrationEdit, MigrationEditKind, MigrationRisk, PlannedOperation,
    RecoveryClass, RiskSeverity, VerificationSpec,
};

use probes::{
    for_each_jsonl_value, is_probably_text_state_file, open_sqlite_read_only,
    open_sqlite_read_write, scan_text_refs, text_file_contains_any, TextScan, MAX_SCAN_BYTES,
    MAX_SCAN_FILES,
};

pub fn live_panes_from_conflicts(conflicts: &[ConflictingPane]) -> Vec<LivePaneImpact> {
    conflicts
        .iter()
        .map(|conflict| {
            let state = format!("{:?}", conflict.state);
            let migratable = matches!(
                conflict.state,
                crate::ActivityState::Idle
                    | crate::ActivityState::AwaitingInput
                    | crate::ActivityState::PlanApproval
                    | crate::ActivityState::Unknown
            );
            LivePaneImpact {
                pane_id: conflict.pane.id(),
                socket: conflict.pane.socket().to_string(),
                harness: conflict.pane.agent_kind,
                session_id: conflict.pane.session_id.clone(),
                cwd: conflict.pane.cwd.clone(),
                relative_path: conflict.relative_path.clone(),
                state,
                migratable,
            }
        })
        .collect()
}

pub fn plan_migration(
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    let context = HarnessOpsContext::system()?;
    planner::plan_migration_with_context_and_scope(
        &context,
        old_path,
        new_path,
        live_panes,
        planner::MigrationPlanScope::Doctor,
    )
}

pub fn plan_migration_apply_ready(
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    let context = HarnessOpsContext::system()?;
    planner::plan_migration_with_context_and_scope(
        &context,
        old_path,
        new_path,
        live_panes,
        planner::MigrationPlanScope::Apply,
    )
}

pub fn plan_migration_with_context(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    planner::plan_migration_with_context_and_scope(
        context,
        old_path,
        new_path,
        live_panes,
        planner::MigrationPlanScope::Doctor,
    )
}

#[cfg(test)]
mod tests;
