//! Project migration command.
//!
//! Project moves are executed through the universal migration planner plus the
//! transaction executor. Harness adapters only describe typed storage edits; the
//! executor owns backup, verification, and rollback.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use claude_babel::agent_kind::AgentKind;
use claude_babel::core::BabelCore;
use claude_babel::harness_ops::{
    apply_migration_plan, plan_migration_apply_ready, AdapterReadiness, HarnessMigrationReport,
    MigrationApplyOptions, MigrationEdit, RecoveryClass, RiskSeverity,
};

pub fn expand_tilde(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if let Some(stripped) = path_str.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }

    path.to_path_buf()
}

pub fn resolve_destination(source: &Path, dest: &Path) -> PathBuf {
    if dest.is_dir() {
        if let Some(name) = source.file_name() {
            return dest.join(name);
        }
    }
    dest.to_path_buf()
}

pub async fn cmd_mv(
    core: &mut BabelCore,
    source: PathBuf,
    dest: PathBuf,
    dry_run: bool,
    history_only: bool,
    anxious: bool,
    force: bool,
    json: bool,
) -> Result<()> {
    let source = expand_tilde(&source);
    let dest = resolve_destination(&source, &expand_tilde(&dest));
    tracing::debug!(
        source = %source.display(),
        dest = %dest.display(),
        dry_run,
        history_only,
        force,
        json,
        "mv: command resolved paths"
    );

    if anxious {
        bail!("babel mv --anxious is not implemented for the universal migration executor yet");
    }

    tracing::debug!("mv: collecting live panes for conflict scan");
    let panes = core.panes().await?;
    tracing::debug!(pane_count = panes.len(), "mv: live pane scan complete");
    let live_panes = super::doctor::live_panes_from_panes(&source, panes);
    tracing::debug!(
        live_pane_impacts = live_panes.len(),
        "mv: planning apply-ready migration"
    );
    let mut plan = plan_migration_apply_ready(&source, &dest, live_panes)?;
    tracing::debug!(
        harnesses = plan.harnesses.len(),
        risks = plan.risks.len(),
        blockers = plan
            .risks
            .iter()
            .filter(|risk| matches!(risk.severity, RiskSeverity::Blocker))
            .count(),
        operations = plan.operations().len(),
        "mv: migration plan complete"
    );

    if plan.has_blockers() && !force {
        let blockers = plan
            .risks
            .iter()
            .filter(|risk| matches!(risk.severity, RiskSeverity::Blocker))
            .map(|risk| risk.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "babel mv blocked by migration risks:\n{}\nRun `babel mv --doctor {} {}` for the full report.",
            blockers,
            source.display(),
            dest.display()
        );
    }

    if !history_only {
        tracing::debug!("mv: appending project directory move operation");
        plan.harnesses.push(HarnessMigrationReport::from_edits(
            AgentKind::Other,
            AdapterReadiness::ApplyReady,
            vec![source.clone()],
            0,
            0,
            vec![MigrationEdit::rename_path(
                AgentKind::Other,
                "move_project_directory",
                source.clone(),
                dest.clone(),
                "move source project directory",
            )
            .with_apply_ready(true)
            .with_recovery(RecoveryClass::OwnedDir)],
            Vec::new(),
        ));
    } else {
        tracing::debug!("mv: history-only mode skips project directory move operation");
    }

    tracing::debug!("mv: applying migration plan");
    let report = apply_migration_plan(
        &plan,
        &MigrationApplyOptions {
            dry_run,
            force: false,
            transaction_root: None,
        },
    )?;
    tracing::debug!(
        dry_run = report.dry_run,
        edits_seen = report.edits_seen,
        edits_apply_ready = report.edits_apply_ready,
        applied = report.applied.len(),
        skipped = report.skipped.len(),
        blockers = report.blockers.len(),
        verified = report.verified.len(),
        rolled_back = report.rolled_back,
        manifest = report
            .manifest_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        "mv: migration apply complete"
    );

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if dry_run {
        println!(
            "babel mv --dry: {} executor-owned edit(s) are apply-ready",
            report.edits_apply_ready
        );
    } else {
        println!(
            "babel mv: applied {} edit(s), verified {}",
            report.applied.len(),
            report.verified.len()
        );
        if let Some(path) = report.manifest_path {
            println!("manifest: {}", path.display());
        }
    }
    if !report.skipped.is_empty() {
        println!("preserved/skipped: {}", report.skipped.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_directory_destination_keeps_source_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("old-project");
        let dest_parent = tmp.path().join("new-parent");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&dest_parent).unwrap();

        assert_eq!(
            resolve_destination(&source, &dest_parent),
            dest_parent.join("old-project")
        );
    }

    #[test]
    fn explicit_destination_path_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("old-project");
        let explicit = tmp.path().join("new-parent/renamed-project");
        std::fs::create_dir_all(&source).unwrap();

        assert_eq!(resolve_destination(&source, &explicit), explicit);
    }
}
