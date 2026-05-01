//! Harness-aware operations planning.
//!
//! This is the read side of the "consume the boards" plan: Babel can inspect
//! provider-native state and produce an operation graph without making a global
//! search index the source of truth. Native harness storage remains authoritative;
//! any future cache must be rebuildable from these adapters.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent_kind::{AgentKind, HarnessSupport};
use crate::core::ConflictingPane;

mod aider;
mod amp;
mod antigravity;
mod apply;
mod claude;
mod cline;
mod codex;
mod crush;
mod cursor;
mod factory;
mod gemini;
mod github_copilot;
mod kilo;
mod kimi;
mod kiro;
mod opencode;
mod qwen;
mod roo;

pub use apply::{apply_migration_plan, MigrationApplyOptions, MigrationApplyReport};

const MAX_SCAN_FILES: usize = 5_000;
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const LARGE_FILE_SAMPLE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct HarnessOpsContext {
    pub home: PathBuf,
    codex_sqlite_home_env: Option<PathBuf>,
}

impl HarnessOpsContext {
    pub fn from_home(home: PathBuf) -> Self {
        Self {
            home,
            codex_sqlite_home_env: None,
        }
    }

    pub fn system() -> Result<Self> {
        Ok(Self {
            home: dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?,
            codex_sqlite_home_env: env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from),
        })
    }

    pub(super) fn claude_base(&self) -> PathBuf {
        self.home.join(".claude")
    }

    pub(super) fn codex_base(&self) -> PathBuf {
        self.home.join(".codex")
    }

    pub(super) fn codex_sessions(&self) -> PathBuf {
        self.home.join(".codex/sessions")
    }

    pub(super) fn codex_archived_sessions(&self) -> PathBuf {
        self.home.join(".codex/archived_sessions")
    }

    pub(super) fn codex_shell_snapshots(&self) -> PathBuf {
        self.home.join(".codex/shell_snapshots")
    }

    pub(super) fn codex_sqlite_home_env(&self) -> Option<PathBuf> {
        self.codex_sqlite_home_env.clone()
    }

    pub(super) fn qwen_base(&self) -> PathBuf {
        self.home.join(".qwen")
    }

    pub(super) fn gemini_tmp(&self) -> PathBuf {
        self.home.join(".gemini/tmp")
    }

    pub(super) fn cursor_roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".cursor/projects"),
            self.home.join(".cursor/chats"),
            self.home
                .join(".config/Cursor/User/globalStorage/state.vscdb"),
            self.home.join(".config/Cursor/User/workspaceStorage"),
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterReadiness {
    /// A future apply engine has a complete transaction contract for this adapter.
    ApplyReady,
    /// The adapter can produce a useful operation graph, but does not mutate.
    DoctorOnly,
    /// Babel knows where to look, but path rewrite semantics are not specified.
    ReconOnly,
    /// No credible storage migration surface is known.
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSeverity {
    Info,
    Warning,
    Blocker,
}

impl RiskSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            RiskSeverity::Info => "info",
            RiskSeverity::Warning => "warn",
            RiskSeverity::Blocker => "blocker",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRisk {
    pub severity: RiskSeverity,
    pub harness: Option<AgentKind>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedOperation {
    pub harness: AgentKind,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub capability: ApplyCapability,
    pub recovery: RecoveryClass,
    pub verification: VerificationSpec,
    pub apply_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationEdit {
    pub harness: AgentKind,
    pub action: String,
    pub kind: MigrationEditKind,
    pub capability: ApplyCapability,
    pub recovery: RecoveryClass,
    pub verification: VerificationSpec,
    pub apply_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyCapability {
    ApplyReady,
    DoctorOnly,
    PreserveOnly,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClass {
    OwnedFile,
    OwnedDir,
    SessionDependencyFile,
    SessionDependencyDir,
    ProjectLocalFollowsMove,
    SqliteSnapshotOnly,
    SqliteClosedAppReplace,
    SharedStateUnsupported,
    PreserveOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationSpec {
    PathMoved {
        from: PathBuf,
        to: PathBuf,
    },
    JsonlFieldRewritten {
        path: PathBuf,
        files: Vec<PathBuf>,
        selector: String,
        from: String,
        to: String,
        expected_count: usize,
    },
    TomlKeyMoved {
        path: PathBuf,
        table: String,
        from_key: String,
        to_key: String,
    },
    TextRefsReduced {
        target: String,
        files: Vec<PathBuf>,
        from: String,
        to: String,
        expected_removed_min: usize,
    },
    SqliteTextColumnRewritten {
        path: PathBuf,
        table: String,
        column: String,
        from: String,
        to: String,
        expected_count: usize,
    },
    SessionCountPreserved {
        harness: AgentKind,
        count: usize,
    },
    PreserveOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationEditKind {
    RenamePath {
        from: PathBuf,
        to: PathBuf,
        preserve: String,
    },
    RewriteJsonlField {
        path: PathBuf,
        files: Vec<PathBuf>,
        selector: String,
        from: String,
        to: String,
        count: usize,
    },
    RewriteTomlTableKey {
        path: PathBuf,
        table: String,
        from_key: String,
        to_key: String,
        count: usize,
    },
    RewriteTextRefs {
        target: String,
        files: Vec<PathBuf>,
        from: String,
        to: String,
        count: usize,
    },
    RewriteSqliteTextColumn {
        path: PathBuf,
        table: String,
        column: String,
        from: String,
        to: String,
        count: usize,
    },
    PreserveSessionKeyedFiles {
        root: PathBuf,
        session_count: usize,
        path_ref_count: usize,
    },
    PreserveProjectLocalHistory {
        target: String,
        detail: String,
    },
}

impl MigrationEdit {
    pub fn rename_path(
        harness: AgentKind,
        action: impl Into<String>,
        from: PathBuf,
        to: PathBuf,
        preserve: impl Into<String>,
    ) -> Self {
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::RenamePath {
                from: from.clone(),
                to: to.clone(),
                preserve: preserve.into(),
            },
            capability: ApplyCapability::DoctorOnly,
            recovery: RecoveryClass::OwnedDir,
            verification: VerificationSpec::PathMoved { from, to },
            apply_ready: false,
        }
    }

    pub fn rewrite_jsonl_field(
        harness: AgentKind,
        action: impl Into<String>,
        path: PathBuf,
        selector: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        count: usize,
    ) -> Self {
        Self::rewrite_jsonl_field_in_files(
            harness,
            action,
            path,
            Vec::new(),
            selector,
            from,
            to,
            count,
        )
    }

    pub fn rewrite_jsonl_field_in_files(
        harness: AgentKind,
        action: impl Into<String>,
        path: PathBuf,
        files: Vec<PathBuf>,
        selector: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        count: usize,
    ) -> Self {
        let path: PathBuf = path;
        let selector = selector.into();
        let from = from.into();
        let to = to.into();
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::RewriteJsonlField {
                path: path.clone(),
                files: files.clone(),
                selector: selector.clone(),
                from: from.clone(),
                to: to.clone(),
                count,
            },
            capability: ApplyCapability::DoctorOnly,
            recovery: RecoveryClass::OwnedFile,
            verification: VerificationSpec::JsonlFieldRewritten {
                path,
                files,
                selector,
                from,
                to,
                expected_count: count,
            },
            apply_ready: false,
        }
    }

    pub fn rewrite_toml_table_key(
        harness: AgentKind,
        action: impl Into<String>,
        path: PathBuf,
        table: impl Into<String>,
        from_key: impl Into<String>,
        to_key: impl Into<String>,
        count: usize,
    ) -> Self {
        let path: PathBuf = path;
        let table = table.into();
        let from_key = from_key.into();
        let to_key = to_key.into();
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::RewriteTomlTableKey {
                path: path.clone(),
                table: table.clone(),
                from_key: from_key.clone(),
                to_key: to_key.clone(),
                count,
            },
            capability: ApplyCapability::DoctorOnly,
            recovery: RecoveryClass::OwnedFile,
            verification: VerificationSpec::TomlKeyMoved {
                path,
                table,
                from_key,
                to_key,
            },
            apply_ready: false,
        }
    }

    pub fn rewrite_text_refs(
        harness: AgentKind,
        action: impl Into<String>,
        target: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        count: usize,
    ) -> Self {
        Self::rewrite_text_refs_in_files(harness, action, target, Vec::new(), from, to, count)
    }

    pub fn rewrite_text_refs_in_files(
        harness: AgentKind,
        action: impl Into<String>,
        target: impl Into<String>,
        files: Vec<PathBuf>,
        from: impl Into<String>,
        to: impl Into<String>,
        count: usize,
    ) -> Self {
        let target = target.into();
        let from = from.into();
        let to = to.into();
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::RewriteTextRefs {
                target: target.clone(),
                files: files.clone(),
                from: from.clone(),
                to: to.clone(),
                count,
            },
            capability: ApplyCapability::DoctorOnly,
            recovery: RecoveryClass::OwnedFile,
            verification: VerificationSpec::TextRefsReduced {
                target,
                files,
                from,
                to,
                expected_removed_min: count,
            },
            apply_ready: false,
        }
    }

    pub fn rewrite_sqlite_text_column(
        harness: AgentKind,
        action: impl Into<String>,
        path: PathBuf,
        table: impl Into<String>,
        column: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        count: usize,
    ) -> Self {
        let table = table.into();
        let column = column.into();
        let from = from.into();
        let to = to.into();
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::RewriteSqliteTextColumn {
                path: path.clone(),
                table: table.clone(),
                column: column.clone(),
                from: from.clone(),
                to: to.clone(),
                count,
            },
            capability: ApplyCapability::DoctorOnly,
            recovery: RecoveryClass::SessionDependencyFile,
            verification: VerificationSpec::SqliteTextColumnRewritten {
                path,
                table,
                column,
                from,
                to,
                expected_count: count,
            },
            apply_ready: false,
        }
    }

    pub fn preserve_session_keyed_files(
        harness: AgentKind,
        action: impl Into<String>,
        root: PathBuf,
        session_count: usize,
        path_ref_count: usize,
    ) -> Self {
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::PreserveSessionKeyedFiles {
                root,
                session_count,
                path_ref_count,
            },
            capability: ApplyCapability::PreserveOnly,
            recovery: RecoveryClass::PreserveOnly,
            verification: VerificationSpec::SessionCountPreserved {
                harness,
                count: session_count,
            },
            apply_ready: false,
        }
    }

    pub fn preserve_project_local_history(
        harness: AgentKind,
        target: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::preserve_project_local_history_action(
            harness,
            "preserve_project_local_history",
            target,
            detail,
        )
    }

    pub fn preserve_project_local_history_action(
        harness: AgentKind,
        action: impl Into<String>,
        target: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            harness,
            action: action.into(),
            kind: MigrationEditKind::PreserveProjectLocalHistory {
                target: target.into(),
                detail: detail.into(),
            },
            capability: ApplyCapability::PreserveOnly,
            recovery: RecoveryClass::ProjectLocalFollowsMove,
            verification: VerificationSpec::PreserveOnly,
            apply_ready: false,
        }
    }

    pub fn with_apply_ready(mut self, apply_ready: bool) -> Self {
        self.apply_ready = apply_ready;
        self.capability = if apply_ready {
            ApplyCapability::ApplyReady
        } else {
            ApplyCapability::DoctorOnly
        };
        self
    }

    pub fn with_recovery(mut self, recovery: RecoveryClass) -> Self {
        self.recovery = recovery;
        self
    }

    pub fn with_capability(mut self, capability: ApplyCapability) -> Self {
        self.apply_ready = matches!(capability, ApplyCapability::ApplyReady);
        self.capability = capability;
        self
    }

    fn target(&self) -> String {
        match &self.kind {
            MigrationEditKind::RenamePath { from, to, .. } => {
                format!("{} -> {}", from.display(), to.display())
            }
            MigrationEditKind::RewriteJsonlField { path, .. }
            | MigrationEditKind::RewriteTomlTableKey { path, .. }
            | MigrationEditKind::RewriteSqliteTextColumn { path, .. } => path.display().to_string(),
            MigrationEditKind::RewriteTextRefs { target, .. }
            | MigrationEditKind::PreserveProjectLocalHistory { target, .. } => target.clone(),
            MigrationEditKind::PreserveSessionKeyedFiles { root, .. } => root.display().to_string(),
        }
    }

    fn detail(&self) -> String {
        match &self.kind {
            MigrationEditKind::RenamePath { preserve, .. } => preserve.clone(),
            MigrationEditKind::RewriteJsonlField {
                selector,
                count,
                files,
                ..
            } => {
                if files.is_empty() {
                    format!("rewrite {count} JSONL record(s) at {selector}")
                } else {
                    format!("rewrite {count} JSONL record(s) at {selector} in pre-scanned file(s)")
                }
            }
            MigrationEditKind::RewriteTomlTableKey {
                table,
                from_key,
                to_key,
                count,
                ..
            } => {
                format!("rewrite {count} TOML [{table}] key(s): {from_key} -> {to_key}")
            }
            MigrationEditKind::RewriteTextRefs { count, files, .. } => {
                if files.is_empty() {
                    format!("rewrite {count} text target(s) containing source path references")
                } else {
                    format!(
                        "rewrite {count} pre-scanned text file(s) containing source path references"
                    )
                }
            }
            MigrationEditKind::RewriteSqliteTextColumn {
                table,
                column,
                count,
                ..
            } => {
                format!("rewrite {count} SQLite row(s) at {table}.{column}")
            }
            MigrationEditKind::PreserveSessionKeyedFiles {
                session_count,
                path_ref_count,
                ..
            } => {
                format!(
                    "{session_count} session-keyed file(s); {path_ref_count} contain source path refs"
                )
            }
            MigrationEditKind::PreserveProjectLocalHistory { detail, .. } => detail.clone(),
        }
    }
}

impl From<&MigrationEdit> for PlannedOperation {
    fn from(edit: &MigrationEdit) -> Self {
        Self {
            harness: edit.harness,
            action: edit.action.clone(),
            target: edit.target(),
            detail: edit.detail(),
            capability: edit.capability,
            recovery: edit.recovery,
            verification: edit.verification.clone(),
            apply_ready: edit.apply_ready,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMigrationReport {
    pub harness: AgentKind,
    pub readiness: AdapterReadiness,
    pub state_roots: Vec<PathBuf>,
    pub sessions_found: usize,
    pub path_references_found: usize,
    pub edits: Vec<MigrationEdit>,
    pub operations: Vec<PlannedOperation>,
    pub notes: Vec<String>,
}

impl HarnessMigrationReport {
    pub fn from_edits(
        harness: AgentKind,
        readiness: AdapterReadiness,
        state_roots: Vec<PathBuf>,
        sessions_found: usize,
        path_references_found: usize,
        edits: Vec<MigrationEdit>,
        notes: Vec<String>,
    ) -> Self {
        let operations = edits.iter().map(PlannedOperation::from).collect();
        Self {
            harness,
            readiness,
            state_roots,
            sessions_found,
            path_references_found,
            edits,
            operations,
            notes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivePaneImpact {
    pub pane_id: u64,
    pub socket: String,
    pub harness: AgentKind,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub relative_path: PathBuf,
    pub state: String,
    pub migratable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationDoctorReport {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub indexing_policy: String,
    pub live_panes: Vec<LivePaneImpact>,
    pub harnesses: Vec<HarnessMigrationReport>,
    pub risks: Vec<MigrationRisk>,
}

impl MigrationDoctorReport {
    pub fn operations(&self) -> Vec<&PlannedOperation> {
        self.harnesses
            .iter()
            .flat_map(|harness| harness.operations.iter())
            .collect()
    }

    pub fn has_blockers(&self) -> bool {
        self.risks
            .iter()
            .any(|risk| matches!(risk.severity, RiskSeverity::Blocker))
    }

    pub fn warning_count(&self) -> usize {
        self.risks
            .iter()
            .filter(|risk| matches!(risk.severity, RiskSeverity::Warning))
            .count()
    }
}

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
    plan_migration_with_context_and_scope(
        &context,
        old_path,
        new_path,
        live_panes,
        MigrationPlanScope::Doctor,
    )
}

pub fn plan_migration_apply_ready(
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    let context = HarnessOpsContext::system()?;
    plan_migration_with_context_and_scope(
        &context,
        old_path,
        new_path,
        live_panes,
        MigrationPlanScope::Apply,
    )
}

pub fn plan_migration_with_context(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    live_panes: Vec<LivePaneImpact>,
) -> Result<MigrationDoctorReport> {
    plan_migration_with_context_and_scope(
        context,
        old_path,
        new_path,
        live_panes,
        MigrationPlanScope::Doctor,
    )
}

#[derive(Clone, Copy, Debug)]
enum MigrationPlanScope {
    Doctor,
    Apply,
}

fn plan_migration_with_context_and_scope(
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
    tracing::debug!("mv.plan: planning Claude storage");
    harnesses.push(claude::plan(
        context,
        &old_abs,
        &new_abs,
        &path_needles,
        &mut risks,
    )?);
    tracing::debug!("mv.plan: planning Codex storage");
    harnesses.push(codex::plan(
        context,
        &old_abs,
        &new_abs,
        &path_needles,
        codex::CodexDiscoveryMode::Indexed,
    )?);
    tracing::debug!("mv.plan: planning Aider storage");
    harnesses.push(aider::plan_for_source(&old_abs));

    if matches!(scope, MigrationPlanScope::Doctor) {
        tracing::debug!("mv.plan: planning full doctor-only harness roster");
        harnesses.push(factory::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(qwen::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(kimi::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(gemini::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(crush::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(cursor::plan(context));
        harnesses.push(cline::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(opencode::plan(context));
        harnesses.push(amp::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(kiro::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(github_copilot::plan(context, &path_needles, &mut risks)?);
        harnesses.push(roo::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(kilo::plan(context, &old_abs, &new_abs, &path_needles)?);
        harnesses.push(antigravity::plan(
            context,
            &old_abs,
            &new_abs,
            &path_needles,
        )?);

        for kind in AgentKind::ALL {
            if harnesses.iter().any(|report| report.harness == *kind) {
                continue;
            }
            harnesses.push(HarnessMigrationReport::from_edits(
                *kind,
                AdapterReadiness::Unsupported,
                Vec::new(),
                0,
                0,
                Vec::new(),
                vec!["no path-move adapter available".to_string()],
            ));
        }
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

fn plan_claude(
    context: &HarnessOpsContext,
    old_path: &Path,
    new_path: &Path,
    needles: &[String],
    risks: &mut Vec<MigrationRisk>,
) -> Result<HarnessMigrationReport> {
    let projects_dir = context.claude_base().join("projects");
    let source_candidates = claude_project_candidates(&projects_dir, old_path);
    let dest_candidates = claude_project_candidates(&projects_dir, new_path);
    let history_path = context.claude_base().join("history.jsonl");
    let session_keyed_roots = claude_session_keyed_roots(context);
    let user_wide_files = claude_user_wide_files(context);

    let existing_sources: Vec<_> = source_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
        .collect();
    let sessions_found = existing_sources.iter().try_fold(0, |count, candidate| {
        Ok::<_, anyhow::Error>(count + count_jsonl_files(&candidate.path)?)
    })?;
    let history_refs = count_history_refs(&history_path, old_path)?;
    let session_root_scans = session_keyed_roots
        .iter()
        .map(|root| Ok((root.clone(), scan_text_refs(root, needles)?)))
        .collect::<Result<Vec<_>>>()?;
    let user_wide_scans = user_wide_files
        .iter()
        .map(|file| Ok((file.clone(), scan_text_refs(file, needles)?)))
        .collect::<Result<Vec<_>>>()?;
    let session_refs = session_root_scans
        .iter()
        .map(|(_, scan)| scan.path_references_found)
        .sum::<usize>();
    let user_wide_refs = user_wide_scans
        .iter()
        .map(|(_, scan)| scan.path_references_found)
        .sum::<usize>();
    let mut edits = Vec::new();
    let mut notes = Vec::new();

    for source in &existing_sources {
        let Some(dest) = dest_candidates
            .iter()
            .find(|candidate| candidate.scheme == source.scheme)
            .or_else(|| dest_candidates.first())
        else {
            continue;
        };
        edits.push(
            MigrationEdit::rename_path(
                AgentKind::Claude,
                "rename_project_dir",
                source.path.clone(),
                dest.path.clone(),
                format!("preserve {} Claude transcript file(s)", sessions_found),
            )
            .with_apply_ready(true)
            .with_recovery(RecoveryClass::SessionDependencyDir),
        );
    }

    if existing_sources.is_empty() {
        notes.push(format!(
            "Claude project directory not found; probed keys: {}",
            source_candidates
                .iter()
                .map(|candidate| candidate.encoded.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if history_refs > 0 {
        edits.push(
            MigrationEdit::rewrite_jsonl_field(
                AgentKind::Claude,
                "rewrite_history_paths",
                history_path.clone(),
                "$.project",
                old_path.display().to_string(),
                new_path.display().to_string(),
                history_refs,
            )
            .with_apply_ready(true)
            .with_recovery(RecoveryClass::SessionDependencyFile),
        );
    }

    for (root, scan) in &session_root_scans {
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Claude,
                    "rewrite_session_keyed_refs",
                    root.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true)
                .with_recovery(RecoveryClass::SessionDependencyDir),
            );
        }
    }

    for (file, scan) in &user_wide_scans {
        if scan.path_references_found > 0 {
            edits.push(
                MigrationEdit::rewrite_text_refs(
                    AgentKind::Claude,
                    "rewrite_user_wide_refs",
                    file.display().to_string(),
                    old_path.display().to_string(),
                    new_path.display().to_string(),
                    scan.path_references_found,
                )
                .with_apply_ready(true)
                .with_recovery(RecoveryClass::SessionDependencyFile),
            );
        }
    }

    for dest in dest_candidates
        .iter()
        .filter(|candidate| candidate.path.exists())
    {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: Some(AgentKind::Claude),
            message: format!(
                "Claude destination project folder already exists for {} key: {}",
                dest.scheme,
                dest.path.display()
            ),
        });
    }

    if source_candidates
        .iter()
        .zip(dest_candidates.iter())
        .any(|(old, new)| old.encoded == new.encoded)
    {
        risks.push(MigrationRisk {
            severity: RiskSeverity::Blocker,
            harness: Some(AgentKind::Claude),
            message: "Claude source and destination can encode to the same project key".to_string(),
        });
    }

    let mut state_roots = vec![context.claude_base(), context.home.join(".claude.json")];
    state_roots.extend(session_keyed_roots);
    state_roots.extend(user_wide_files);
    state_roots.retain(|path| path.exists());
    state_roots.sort();
    state_roots.dedup();

    Ok(HarnessMigrationReport::from_edits(
        AgentKind::Claude,
        AdapterReadiness::ApplyReady,
        state_roots,
        sessions_found,
        history_refs + session_refs + user_wide_refs,
        edits,
        notes,
    ))
}

#[derive(Debug)]
struct ClaudeProjectCandidate {
    scheme: &'static str,
    encoded: String,
    path: PathBuf,
}

fn claude_project_candidates(
    projects_dir: &Path,
    project_path: &Path,
) -> Vec<ClaudeProjectCandidate> {
    let mut candidates = Vec::new();
    let cc_port = claude_encode_cc_port(project_path);
    candidates.push(ClaudeProjectCandidate {
        scheme: "primary",
        path: projects_dir.join(&cc_port),
        encoded: cc_port,
    });

    let ccmv = claude_encode_ccmv(project_path);
    if candidates.iter().all(|candidate| candidate.encoded != ccmv) {
        candidates.push(ClaudeProjectCandidate {
            scheme: "compat",
            path: projects_dir.join(&ccmv),
            encoded: ccmv,
        });
    }

    candidates
}

fn claude_encode_cc_port(path: &Path) -> String {
    normalized_path_for_key(path)
        .replace('/', "-")
        .replace('.', "-")
        .replace(' ', "-")
}

fn claude_encode_ccmv(path: &Path) -> String {
    let normalized = normalized_path_for_key(path);
    normalized
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn normalized_path_for_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.len() > 1 {
        raw.trim_end_matches('/').to_string()
    } else {
        raw.to_string()
    }
}

fn claude_session_keyed_roots(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.claude_base().join("todos"),
        context.claude_base().join("usage-data/session-meta"),
        context.claude_base().join("usage-data/facets"),
        context.claude_base().join("plugins/data"),
        context.claude_base().join("tasks"),
    ]
}

fn claude_user_wide_files(context: &HarnessOpsContext) -> Vec<PathBuf> {
    vec![
        context.home.join(".claude.json"),
        context.claude_base().join("settings.json"),
        context.claude_base().join("plugins/installed_plugins.json"),
        context
            .claude_base()
            .join("plugins/known_marketplaces.json"),
    ]
}

fn count_jsonl_files(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
            count += 1;
        }
    }
    Ok(count)
}

fn count_history_refs(history_path: &Path, old_path: &Path) -> Result<usize> {
    if !history_path.exists() {
        return Ok(0);
    }

    let content = fs::read_to_string(history_path)?;
    let old_path = old_path.to_string_lossy();
    let child_prefix = format!("{}/", old_path);
    let mut count = 0;

    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(project) = value.get("project").and_then(|v| v.as_str()) else {
            continue;
        };
        if project == old_path || project.starts_with(&child_prefix) {
            count += 1;
        }
    }
    Ok(count)
}

#[derive(Default)]
struct TextScan {
    files_scanned: usize,
    path_references_found: usize,
    truncated: bool,
    large_files_sampled: usize,
}

fn scan_text_refs(root: &Path, needles: &[String]) -> Result<TextScan> {
    let mut scan = TextScan::default();
    if !root.exists() {
        return Ok(scan);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if scan.files_scanned >= MAX_SCAN_FILES {
            scan.truncated = true;
            break;
        }

        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                stack.push(entry.path());
            }
            continue;
        }
        if !metadata.is_file() || !is_probably_text_state_file(&path) {
            continue;
        }

        scan.files_scanned += 1;
        let Ok(found) = text_file_contains_any(&path, metadata.len(), needles) else {
            continue;
        };
        if metadata.len() > MAX_SCAN_BYTES {
            scan.large_files_sampled += 1;
        }
        if found {
            scan.path_references_found += 1;
        }
    }

    Ok(scan)
}

fn text_file_contains_any(path: &Path, len: u64, needles: &[String]) -> Result<bool> {
    if len <= MAX_SCAN_BYTES {
        let content = fs::read_to_string(path)?;
        return Ok(needles.iter().any(|needle| content.contains(needle)));
    }

    let mut file = fs::File::open(path)?;
    let mut head = vec![0; LARGE_FILE_SAMPLE_BYTES.min(len as usize)];
    let head_len = file.read(&mut head)?;
    head.truncate(head_len);
    if contains_any_bytes(&head, needles) {
        return Ok(true);
    }

    if len > LARGE_FILE_SAMPLE_BYTES as u64 {
        let tail_start = len.saturating_sub(LARGE_FILE_SAMPLE_BYTES as u64);
        file.seek(SeekFrom::Start(tail_start))?;
        let mut tail = Vec::with_capacity(LARGE_FILE_SAMPLE_BYTES);
        file.take(LARGE_FILE_SAMPLE_BYTES as u64)
            .read_to_end(&mut tail)?;
        if contains_any_bytes(&tail, needles) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn contains_any_bytes(bytes: &[u8], needles: &[String]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    needles.iter().any(|needle| text.contains(needle))
}

fn is_probably_text_state_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl") | Some("toml") | Some("txt") | Some("md")
    )
}

fn absolute_path(path: &Path) -> PathBuf {
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
    AgentKind::ALL
        .iter()
        .copied()
        .filter(|kind| !matches!(kind.spec().support, HarnessSupport::Unsupported))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn claude_doctor_reports_project_and_history_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("Workspace/old");
        let new = home.join("Workspace/new");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let old_project = ctx
            .claude_base()
            .join("projects")
            .join(claude_encode_cc_port(&old));
        write_file(&old_project.join("session-a.jsonl"), "{}\n");
        write_file(
            &ctx.claude_base().join("history.jsonl"),
            &format!(
                "{{\"project\":\"{}\",\"sessionId\":\"session-a\",\"timestamp\":1,\"display\":\"x\"}}\n",
                old.display()
            ),
        );

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        let claude = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Claude)
            .unwrap();

        assert_eq!(claude.sessions_found, 1);
        assert_eq!(claude.path_references_found, 1);
        assert!(claude
            .operations
            .iter()
            .any(|op| op.action == "rename_project_dir" && op.apply_ready));
        assert!(claude.edits.iter().any(|edit| {
            edit.action == "rename_project_dir"
                && matches!(&edit.kind, MigrationEditKind::RenamePath { .. })
        }));
        assert!(claude
            .operations
            .iter()
            .any(|op| op.action == "rewrite_history_paths" && op.apply_ready));
        assert!(claude.edits.iter().any(|edit| {
            edit.action == "rewrite_history_paths"
                && matches!(&edit.kind, MigrationEditKind::RewriteJsonlField { .. })
        }));
        assert!(!report.has_blockers());
    }

    #[test]
    fn doctor_blocks_nested_destinations_and_existing_claude_target() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("repo");
        let new = old.join("nested");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let new_project = ctx
            .claude_base()
            .join("projects")
            .join(claude_encode_cc_port(&new));
        fs::create_dir_all(new_project).unwrap();

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        assert!(report.has_blockers());
        assert!(report
            .risks
            .iter()
            .any(|risk| risk.message.contains("nested inside source")));
        assert!(report
            .risks
            .iter()
            .any(|risk| risk.message.contains("destination project folder")));
    }

    #[test]
    fn claude_project_keys_ignore_trailing_slashes() {
        let path = Path::new("/home/nuck/holoq/repo-os/claude-babel/");
        assert_eq!(
            claude_encode_cc_port(path),
            "-home-nuck-holoq-repo-os-claude-babel"
        );
        assert_eq!(
            claude_encode_ccmv(path),
            "-home-nuck-holoq-repo-os-claude-babel"
        );
    }

    #[test]
    fn absolute_path_normalizes_dot_dot_without_existing_destination() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_path(Path::new("repo/../repo-tool/pomet")),
            cwd.join("repo-tool/pomet")
        );
    }

    #[test]
    fn codex_uses_native_session_identity_and_project_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("project");
        let new = home.join("project-renamed");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        write_file(
            &ctx.codex_sessions()
                .join("2026/04/29/rollout-2026-04-29T12-00-00-codex-session.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\",\"collaboration_mode\":{{\"mode\":\"plan\"}}}}}}\n",
                old.display(),
                old.display()
            ),
        );
        write_file(
            &ctx.codex_sessions()
                .join("2026/04/29/rollout-2026-04-29T12-00-00-unrelated.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"unrelated\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"{}\"}}}}\n",
                home.join("other").display(),
                old.display()
            ),
        );
        write_file(
            &ctx.codex_base().join("history.jsonl"),
            &format!(
                "{{\"session_id\":\"codex-session\",\"text\":\"{}\"}}\n",
                old.display()
            ),
        );
        write_file(
            &ctx.codex_base().join("config.toml"),
            &format!(
                "sqlite_home = \"{}\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                ctx.codex_base().join("sqlite-home").display(),
                old.display(),
            ),
        );
        {
            let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
            fs::create_dir_all(state_db.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(&state_db).unwrap();
            conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd) VALUES (?1, ?2)",
                rusqlite::params!["codex-session", old.to_string_lossy()],
            )
            .unwrap();
        }
        write_file(
            &ctx.codex_shell_snapshots().join("codex-session.1.sh"),
            &format!("cd {}\n", old.display()),
        );
        write_file(
            &ctx.gemini_tmp().join("hash/chats/session.json"),
            &format!("{{\"project\":\"{}\"}}\n", old.display()),
        );

        let report = plan_migration_with_context(&ctx, &old, &new, Vec::new()).unwrap();
        let codex = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Codex)
            .unwrap();
        assert_eq!(codex.sessions_found, 1);
        assert_eq!(codex.path_references_found, 6);
        assert!(matches!(codex.readiness, AdapterReadiness::ApplyReady));
        assert!(codex.operations.iter().any(|op| op.apply_ready));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_session_meta_cwd"));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_session_meta_cwd"
                && matches!(&edit.kind, MigrationEditKind::RewriteJsonlField { .. })
        }));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_session_path_refs"
                && matches!(
                    &edit.kind,
                    MigrationEditKind::RewriteTextRefs { files, .. }
                    if files.len() == 2
                )
        }));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_project_config_keys"));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_project_config_keys"
                && matches!(&edit.kind, MigrationEditKind::RewriteTomlTableKey { .. })
        }));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "rewrite_thread_index_cwd"
                && op.target.ends_with(".codex/sqlite-home/state_5.sqlite")
                && op.apply_ready));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_thread_index_cwd"
                && matches!(
                    &edit.kind,
                    MigrationEditKind::RewriteSqliteTextColumn { .. }
                )
        }));
        assert!(codex
            .operations
            .iter()
            .any(|op| op.action == "preserve_session_shell_snapshots"));
        assert!(codex.edits.iter().any(|edit| {
            edit.action == "preserve_session_shell_snapshots"
                && matches!(
                    &edit.kind,
                    MigrationEditKind::PreserveSessionKeyedFiles { .. }
                )
        }));

        let gemini = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Gemini)
            .unwrap();
        assert_eq!(gemini.path_references_found, 1);
    }

    #[test]
    fn codex_indexed_planning_uses_thread_index_without_rollout_tree_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("project");
        let new = home.join("project-renamed");
        fs::create_dir_all(&old).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let matched_rollout = ctx
            .codex_sessions()
            .join("2026/04/29/rollout-2026-04-29T12-00-00-codex-session.jsonl");
        write_file(
            &matched_rollout,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n",
                old.display(),
            ),
        );
        write_file(
            &ctx.codex_sessions()
                .join("2026/04/29/rollout-2026-04-29T12-00-00-unrelated.jsonl"),
            &format!(
                "{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"unrelated body mention {}\"}}}}\n",
                old.display(),
            ),
        );
        write_file(
            &ctx.codex_base().join("config.toml"),
            &format!(
                "sqlite_home = \"{}\"\n",
                ctx.codex_base().join("sqlite-home").display(),
            ),
        );
        {
            let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
            fs::create_dir_all(state_db.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(&state_db).unwrap();
            conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "codex-session",
                    matched_rollout.to_string_lossy(),
                    old.to_string_lossy(),
                ],
            )
            .unwrap();
        }

        for scope in [MigrationPlanScope::Apply, MigrationPlanScope::Doctor] {
            let report =
                plan_migration_with_context_and_scope(&ctx, &old, &new, Vec::new(), scope).unwrap();
            let codex = report
                .harnesses
                .iter()
                .find(|harness| harness.harness == AgentKind::Codex)
                .unwrap();

            assert_eq!(codex.sessions_found, 1);
            assert!(!codex
                .operations
                .iter()
                .any(|op| op.action == "rewrite_session_path_refs"));
            assert!(codex.edits.iter().any(|edit| {
                edit.action == "rewrite_session_meta_cwd"
                    && matches!(
                        &edit.kind,
                        MigrationEditKind::RewriteJsonlField { files, .. }
                        if files == &vec![matched_rollout.clone()]
                    )
            }));
        }
    }

    #[test]
    fn codex_apply_repairs_alias_cwd_surfaces_without_original_source() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let old = home.join("repo-tool/pomet");
        let old_alias = home.join("repo/../repo-tool/pomet");
        let new = home.join("repo/pomet");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();

        let ctx = HarnessOpsContext::from_home(home.to_path_buf());
        let matched_rollout = ctx
            .codex_sessions()
            .join("2026/05/01/rollout-2026-05-01T13-09-14-codex-session.jsonl");
        write_file(
            &matched_rollout,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                old_alias.display(),
                new.display(),
            ),
        );
        write_file(
            &ctx.codex_base().join("config.toml"),
            &format!(
                "sqlite_home = \"{}\"\n",
                ctx.codex_base().join("sqlite-home").display(),
            ),
        );
        let state_db = ctx.codex_base().join("sqlite-home/state_5.sqlite");
        {
            fs::create_dir_all(state_db.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(&state_db).unwrap();
            conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, rollout_path, cwd) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "codex-session",
                    matched_rollout.to_string_lossy(),
                    old.to_string_lossy(),
                ],
            )
            .unwrap();
        }

        let report = plan_migration_with_context_and_scope(
            &ctx,
            &old,
            &new,
            Vec::new(),
            MigrationPlanScope::Apply,
        )
        .unwrap();
        let codex = report
            .harnesses
            .iter()
            .find(|harness| harness.harness == AgentKind::Codex)
            .unwrap();

        assert!(codex.edits.iter().any(|edit| {
            edit.action == "rewrite_session_meta_cwd"
                && matches!(
                    &edit.kind,
                    MigrationEditKind::RewriteJsonlField { from, files, .. }
                    if from == &old_alias.display().to_string()
                        && files == &vec![matched_rollout.clone()]
                )
        }));
        assert!(!codex
            .edits
            .iter()
            .any(|edit| edit.action == "rewrite_turn_context_cwd"));

        let apply = apply_migration_plan(
            &report,
            &MigrationApplyOptions {
                dry_run: false,
                force: false,
                transaction_root: Some(home.join("transactions")),
            },
        )
        .unwrap();

        assert!(!apply.has_blockers());
        let rollout = fs::read_to_string(&matched_rollout).unwrap();
        assert!(rollout.contains(&format!("\"cwd\":\"{}\"", new.display())));
        assert!(!rollout.contains(&old_alias.display().to_string()));

        let conn = rusqlite::Connection::open(&state_db).unwrap();
        let cwd: String = conn
            .query_row(
                "SELECT cwd FROM threads WHERE id = 'codex-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cwd, new.display().to_string());
    }

    #[test]
    fn generic_apply_rewrites_only_prescanned_text_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(root.join("state/day")).unwrap();

        let matching = root.join("state/day/matching.jsonl");
        let unrelated = root.join("state/day/unrelated.jsonl");
        write_file(&matching, &format!("cwd={}\n", old.display()));
        write_file(&unrelated, &format!("cwd={}\n", old.display()));

        let edit = MigrationEdit::rewrite_text_refs_in_files(
            AgentKind::Codex,
            "rewrite_session_path_refs",
            root.join("state").display().to_string(),
            vec![matching.clone()],
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true);
        let report = MigrationDoctorReport {
            old_path: old.clone(),
            new_path: new.clone(),
            indexing_policy: "test".to_string(),
            live_panes: Vec::new(),
            harnesses: vec![HarnessMigrationReport::from_edits(
                AgentKind::Codex,
                AdapterReadiness::ApplyReady,
                vec![root.join("state")],
                0,
                1,
                vec![edit],
                Vec::new(),
            )],
            risks: Vec::new(),
        };

        let apply = apply_migration_plan(
            &report,
            &MigrationApplyOptions {
                dry_run: false,
                force: false,
                transaction_root: Some(root.join("transactions")),
            },
        )
        .unwrap();

        assert!(!apply.has_blockers());
        assert_eq!(apply.verified.len(), 1);
        assert!(fs::read_to_string(&matching)
            .unwrap()
            .contains(&new.display().to_string()));
        assert!(fs::read_to_string(&unrelated)
            .unwrap()
            .contains(&old.display().to_string()));
    }

    #[test]
    fn generic_apply_consumes_typed_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();

        let jsonl = root.join("history.jsonl");
        write_file(
            &jsonl,
            &format!(
                "{{\"project\":\"{}\",\"display\":\"x\",\"collaboration_mode\":{{\"mode\":\"plan\"}}}}\n",
                old.display()
            ),
        );
        let toml = root.join("config.toml");
        write_file(
            &toml,
            &format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                old.display()
            ),
        );
        let text = root.join("notes.txt");
        write_file(&text, &format!("cwd={}\n", old.display()));
        let sqlite = root.join("state_5.sqlite");
        {
            let conn = rusqlite::Connection::open(&sqlite).unwrap();
            conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd) VALUES (?1, ?2)",
                rusqlite::params!["session", old.to_string_lossy()],
            )
            .unwrap();
        }

        let edits = vec![
            MigrationEdit::rewrite_jsonl_field(
                AgentKind::Claude,
                "rewrite_history_paths",
                jsonl.clone(),
                "$.project",
                old.display().to_string(),
                new.display().to_string(),
                1,
            )
            .with_apply_ready(true),
            MigrationEdit::rewrite_toml_table_key(
                AgentKind::Codex,
                "rewrite_project_config_keys",
                toml.clone(),
                "projects",
                old.display().to_string(),
                new.display().to_string(),
                1,
            )
            .with_apply_ready(true),
            MigrationEdit::rewrite_text_refs(
                AgentKind::Gemini,
                "rewrite_native_path_refs",
                text.display().to_string(),
                old.display().to_string(),
                new.display().to_string(),
                1,
            )
            .with_apply_ready(true),
            MigrationEdit::rewrite_sqlite_text_column(
                AgentKind::Codex,
                "rewrite_thread_index_cwd",
                sqlite.clone(),
                "threads",
                "cwd",
                old.display().to_string(),
                new.display().to_string(),
                1,
            )
            .with_apply_ready(true),
        ];
        let report = MigrationDoctorReport {
            old_path: old.clone(),
            new_path: new.clone(),
            indexing_policy: "test".to_string(),
            live_panes: Vec::new(),
            harnesses: vec![HarnessMigrationReport::from_edits(
                AgentKind::Claude,
                AdapterReadiness::ApplyReady,
                vec![root.to_path_buf()],
                0,
                4,
                edits,
                Vec::new(),
            )],
            risks: Vec::new(),
        };

        let apply = apply_migration_plan(
            &report,
            &MigrationApplyOptions {
                dry_run: false,
                force: false,
                transaction_root: Some(root.join("transactions")),
            },
        )
        .unwrap();
        assert_eq!(apply.edits_seen, 4);
        assert!(!apply.has_blockers());
        assert!(fs::read_to_string(&jsonl)
            .unwrap()
            .contains(&new.display().to_string()));
        assert!(fs::read_to_string(&jsonl)
            .unwrap()
            .contains("\"mode\":\"plan\""));
        assert!(fs::read_to_string(&toml)
            .unwrap()
            .contains(&new.display().to_string()));
        assert!(fs::read_to_string(&text)
            .unwrap()
            .contains(&new.display().to_string()));
        let conn = rusqlite::Connection::open(sqlite).unwrap();
        let cwd: String = conn
            .query_row("SELECT cwd FROM threads WHERE id = 'session'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cwd, new.to_string_lossy().to_string());
        assert!(apply.manifest_path.unwrap().exists());
        assert_eq!(apply.verified.len(), 4);
    }

    #[test]
    fn generic_apply_skips_preserve_only_edits_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let report = MigrationDoctorReport {
            old_path: root.join("old"),
            new_path: root.join("new"),
            indexing_policy: "test".to_string(),
            live_panes: Vec::new(),
            harnesses: vec![HarnessMigrationReport::from_edits(
                AgentKind::Aider,
                AdapterReadiness::DoctorOnly,
                vec![root.to_path_buf()],
                1,
                0,
                vec![MigrationEdit::preserve_project_local_history(
                    AgentKind::Aider,
                    root.display().to_string(),
                    "project-local files follow the move",
                )],
                Vec::new(),
            )],
            risks: Vec::new(),
        };

        let apply = apply_migration_plan(
            &report,
            &MigrationApplyOptions {
                dry_run: false,
                force: false,
                transaction_root: Some(root.join("transactions")),
            },
        )
        .unwrap();

        assert_eq!(apply.edits_seen, 1);
        assert_eq!(apply.edits_apply_ready, 0);
        assert!(!apply.has_blockers());
        assert!(apply.manifest_path.is_none());
        assert_eq!(apply.skipped.len(), 1);
    }

    #[test]
    fn generic_apply_rolls_back_owned_files_when_verification_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();
        let text = root.join("notes.txt");
        write_file(&text, &format!("cwd={}\n", old.display()));

        let mut edit = MigrationEdit::rewrite_text_refs(
            AgentKind::Gemini,
            "rewrite_native_path_refs",
            text.display().to_string(),
            old.display().to_string(),
            new.display().to_string(),
            1,
        )
        .with_apply_ready(true);
        edit.verification = VerificationSpec::TextRefsReduced {
            target: text.display().to_string(),
            files: Vec::new(),
            from: new.display().to_string(),
            to: old.display().to_string(),
            expected_removed_min: 1,
        };

        let report = MigrationDoctorReport {
            old_path: old.clone(),
            new_path: new,
            indexing_policy: "test".to_string(),
            live_panes: Vec::new(),
            harnesses: vec![HarnessMigrationReport::from_edits(
                AgentKind::Gemini,
                AdapterReadiness::ApplyReady,
                vec![root.to_path_buf()],
                0,
                1,
                vec![edit],
                Vec::new(),
            )],
            risks: Vec::new(),
        };

        let error = apply_migration_plan(
            &report,
            &MigrationApplyOptions {
                dry_run: false,
                force: false,
                transaction_root: Some(root.join("transactions")),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("rolled back"));
        assert_eq!(
            fs::read_to_string(&text).unwrap(),
            format!("cwd={}\n", old.display())
        );
    }
}
