//! Shared migration domain DTOs for harness operations.
//!
//! These types are the contract between the harness adapters (`claude`,
//! `codex`, `apply`, ...) and the public migration commands. Extracted from
//! `harness_ops.rs` so the module root can stay focused on context, planner
//! registration, and orchestration; submodules and external callers continue
//! to import them from `crate::harness_ops` via root re-exports.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent_kind::AgentKind;

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

    pub(super) fn target(&self) -> String {
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

    pub(super) fn detail(&self) -> String {
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
