//! Migration planner registry and dispatch pipeline.
//!
//! Each `AgentKind` is wired through a `MigrationPlanner` entry that names how
//! it participates in the two scopes (`Doctor` reports, `Apply` mutates) and
//! the function that turns the resolved (`old`, `new`) pair into a per-harness
//! report. The registry is the single source of truth: ordering, scope gating,
//! and supported-roster filtering all derive from `MIGRATION_PLANNERS`.
//!
//! Sibling adapter modules under `harness_ops::*` own provider-native logic;
//! this module only fans the same inputs across them and aggregates risks.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::agent_kind::{AgentKind, HarnessSupport};

use super::{
    aider, amp, antigravity, claude, cline, codex, crush, cursor, factory, gemini, github_copilot,
    kilo, kimi, kiro, opencode, qwen, roo, HarnessMigrationReport, HarnessOpsContext,
    LivePaneImpact, MigrationDoctorReport, MigrationRisk, RiskSeverity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MigrationPlanScope {
    Doctor,
    Apply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlannerAvailability {
    ApplyAndDoctor,
    DoctorOnly,
}

type MigrationPlannerFn =
    for<'a> fn(&mut MigrationPlannerInput<'a>) -> Result<HarnessMigrationReport>;

pub(super) struct MigrationPlanner {
    pub(super) kind: AgentKind,
    availability: PlannerAvailability,
    plan: MigrationPlannerFn,
}

impl MigrationPlanner {
    const fn apply_and_doctor(kind: AgentKind, plan: MigrationPlannerFn) -> Self {
        Self {
            kind,
            availability: PlannerAvailability::ApplyAndDoctor,
            plan,
        }
    }

    const fn doctor_only(kind: AgentKind, plan: MigrationPlannerFn) -> Self {
        Self {
            kind,
            availability: PlannerAvailability::DoctorOnly,
            plan,
        }
    }

    fn runs_in(&self, scope: MigrationPlanScope) -> bool {
        matches!(
            (scope, self.availability),
            (MigrationPlanScope::Doctor, _)
                | (
                    MigrationPlanScope::Apply,
                    PlannerAvailability::ApplyAndDoctor
                )
        )
    }
}

struct MigrationPlannerInput<'a> {
    context: &'a HarnessOpsContext,
    old_abs: &'a Path,
    new_abs: &'a Path,
    path_needles: &'a [String],
    risks: &'a mut Vec<MigrationRisk>,
}

pub(super) const MIGRATION_PLANNERS: &[MigrationPlanner] = &[
    MigrationPlanner::apply_and_doctor(AgentKind::Claude, plan_claude_entry),
    MigrationPlanner::apply_and_doctor(AgentKind::Codex, plan_codex_entry),
    MigrationPlanner::apply_and_doctor(AgentKind::Aider, plan_aider_entry),
    MigrationPlanner::doctor_only(AgentKind::FactoryDroid, plan_factory_entry),
    MigrationPlanner::doctor_only(AgentKind::QwenCode, plan_qwen_entry),
    MigrationPlanner::doctor_only(AgentKind::Kimi, plan_kimi_entry),
    MigrationPlanner::doctor_only(AgentKind::Gemini, plan_gemini_entry),
    MigrationPlanner::doctor_only(AgentKind::Crush, plan_crush_entry),
    MigrationPlanner::doctor_only(AgentKind::Cursor, plan_cursor_entry),
    MigrationPlanner::doctor_only(AgentKind::Cline, plan_cline_entry),
    MigrationPlanner::doctor_only(AgentKind::OpenCode, plan_opencode_entry),
    MigrationPlanner::doctor_only(AgentKind::Amp, plan_amp_entry),
    MigrationPlanner::doctor_only(AgentKind::Kiro, plan_kiro_entry),
    MigrationPlanner::doctor_only(AgentKind::GithubCopilot, plan_github_copilot_entry),
    MigrationPlanner::doctor_only(AgentKind::RooCode, plan_roo_entry),
    MigrationPlanner::doctor_only(AgentKind::KiloCode, plan_kilo_entry),
    MigrationPlanner::doctor_only(AgentKind::Antigravity, plan_antigravity_entry),
];

pub(super) fn migration_planners_for_scope(
    scope: MigrationPlanScope,
) -> impl Iterator<Item = &'static MigrationPlanner> {
    MIGRATION_PLANNERS
        .iter()
        .filter(move |planner| planner.runs_in(scope))
}

fn plan_claude_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    claude::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
        input.risks,
    )
}

fn plan_codex_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    codex::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
        codex::CodexDiscoveryMode::Indexed,
    )
}

fn plan_aider_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    Ok(aider::plan_for_source(input.old_abs))
}

fn plan_factory_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    factory::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_qwen_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    qwen::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_kimi_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    kimi::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_gemini_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    gemini::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_crush_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    crush::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_cursor_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    Ok(cursor::plan(input.context))
}

fn plan_cline_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    cline::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_opencode_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    Ok(opencode::plan(input.context))
}

fn plan_amp_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    amp::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_kiro_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    kiro::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_github_copilot_entry(
    input: &mut MigrationPlannerInput<'_>,
) -> Result<HarnessMigrationReport> {
    github_copilot::plan(input.context, input.path_needles, input.risks)
}

fn plan_roo_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    roo::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_kilo_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    kilo::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

fn plan_antigravity_entry(input: &mut MigrationPlannerInput<'_>) -> Result<HarnessMigrationReport> {
    antigravity::plan(
        input.context,
        input.old_abs,
        input.new_abs,
        input.path_needles,
    )
}

pub(super) fn plan_migration_with_context_and_scope(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
    scope: MigrationPlanScope,
) -> Result<MigrationDoctorReport> {
    tracing::debug!(
        old_path = %old_path.display(),
        new_path = %new_path.display(),
        live_panes = live_panes.len(),
        scope = ?scope,
        "mv.plan: starting migration planning"
    );
    let old_abs = absolute_path(old_path);
    let new_abs = absolute_path(new_path);
    let path_needles = path_needles(old_path, &old_abs);

    let mut risks = Vec::new();
    if old_abs == new_abs {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: None,
            message: "source and destination resolve to the same path".to_string(),
        });
    }
    if new_abs.starts_with(&old_abs) {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: None,
            message: "destination is nested inside source".to_string(),
        });
    }

    for pane in &live_panes {
        if !pane.migratable {
            risks.push(MigrationRisk {
                severity: RiskSeverity::Blocker,
                harness: Some(pane.harness),
                message: format!(
                    "pane {} is active ({}) and would break during migration",
                    pane.pane_id, pane.state
                ),
            });
        }
    }

    let mut harnesses = Vec::new();
    for planner in migration_planners_for_scope(scope) {
        tracing::debug!(
            harness = %planner.kind,
            availability = ?planner.availability,
            "mv.plan: planning harness storage"
        );
        let mut input = MigrationPlannerInput {
            context,
            old_abs: &old_abs,
            new_abs: &new_abs,
            path_needles: &path_needles,
            risks: &mut risks,
        };
        harnesses.push((planner.plan)(&mut input)?);
    }

    risks.push(MigrationRisk {
        severity: RiskSeverity::Info,
        harness: None,
        message: match scope {
            MigrationPlanScope::Doctor => {
                "no cross-harness full-text index is used; all counts come from native storage scans"
            }
            MigrationPlanScope::Apply => {
                "apply mode scans executor-ready adapters and blocker gates only; run mv --doctor for the full harness report"
            }
        }
        .to_string(),
    });

    tracing::debug!(
        harnesses = harnesses.len(),
        risks = risks.len(),
        operations = harnesses
            .iter()
            .map(|harness| harness.operations.len())
            .sum::<usize>(),
        edits = harnesses
            .iter()
            .map(|harness| harness.edits.len())
            .sum::<usize>(),
        "mv.plan: migration planning complete"
    );
    Ok(MigrationDoctorReport {
        old_path: old_abs,
        new_path: new_abs,
        indexing_policy:
            "native storage is source of truth; indexing is deferred and must remain rebuildable"
                .to_string(),
        live_panes,
        harnesses,
        risks,
    })
}

pub(super) fn absolute_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if path.is_absolute() {
        return normalize_lexical_path(path);
    }
    let absolute = std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf());
    normalize_lexical_path(&absolute)
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(value) => normalized.push(value),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn path_needles(original: &Path, absolute: &Path) -> Vec<String> {
    let mut needles = BTreeSet::new();
    needles.insert(absolute.to_string_lossy().to_string());
    if original.is_absolute() {
        needles.insert(original.to_string_lossy().to_string());
    }
    needles.into_iter().collect()
}

pub fn supported_operation_harnesses() -> Vec<AgentKind> {
    MIGRATION_PLANNERS
        .iter()
        .map(|planner| planner.kind)
        .filter(|kind| !matches!(kind.spec().support, HarnessSupport::Unsupported))
        .collect()
}
