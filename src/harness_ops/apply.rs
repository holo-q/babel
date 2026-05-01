use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use filetime::{set_file_times, FileTime};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::{
    ApplyCapability, MigrationDoctorReport, MigrationEdit, MigrationEditKind, RecoveryClass,
    VerificationSpec,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationApplyOptions {
    pub dry_run: bool,
    pub force: bool,
    pub transaction_root: Option<PathBuf>,
    #[serde(default)]
    pub print_progress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationApplyReport {
    pub dry_run: bool,
    pub edits_seen: usize,
    pub edits_apply_ready: usize,
    pub manifest_path: Option<PathBuf>,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub blockers: Vec<String>,
    pub verified: Vec<String>,
    pub rolled_back: bool,
}

impl MigrationApplyReport {
    fn new(options: &MigrationApplyOptions, edits_seen: usize) -> Self {
        Self {
            dry_run: options.dry_run,
            edits_seen,
            edits_apply_ready: 0,
            manifest_path: None,
            applied: Vec::new(),
            skipped: Vec::new(),
            blockers: Vec::new(),
            verified: Vec::new(),
            rolled_back: false,
        }
    }

    pub fn has_blockers(&self) -> bool {
        !self.blockers.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionManifest {
    id: String,
    status: TransactionStatus,
    old_path: PathBuf,
    new_path: PathBuf,
    edits_total: usize,
    backups: Vec<BackupRecord>,
    events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionStatus {
    Planned,
    Applying,
    Verifying,
    Complete,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupRecord {
    kind: BackupKind,
    target: PathBuf,
    backup: Option<PathBuf>,
    existed: bool,
    before_checksum: Option<String>,
    after_checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackupKind {
    File,
    Rename { from: PathBuf, to: PathBuf },
}

struct MigrationTransaction {
    dir: PathBuf,
    manifest_path: PathBuf,
    manifest: TransactionManifest,
    progress: Option<ProgressBar>,
}

impl MigrationTransaction {
    fn start(plan: &MigrationDoctorReport, options: &MigrationApplyOptions) -> Result<Self> {
        let id = migration_id(&plan.old_path, &plan.new_path);
        let root = options
            .transaction_root
            .clone()
            .unwrap_or_else(default_transaction_root);
        let dir = root.join(&id);
        let backups = dir.join("backups");
        tracing::debug!(
            migration_id = %id,
            transaction_dir = %dir.display(),
            backup_dir = %backups.display(),
            old_path = %plan.old_path.display(),
            new_path = %plan.new_path.display(),
            "mv.apply: starting transaction"
        );
        fs::create_dir_all(&backups)
            .with_context(|| format!("failed to create {}", backups.display()))?;
        let manifest_path = dir.join("manifest.json");
        let tx = Self {
            dir,
            manifest_path,
            progress: None,
            manifest: TransactionManifest {
                id,
                status: TransactionStatus::Planned,
                old_path: plan.old_path.clone(),
                new_path: plan.new_path.clone(),
                edits_total: plan
                    .harnesses
                    .iter()
                    .map(|harness| harness.edits.len())
                    .sum(),
                backups: Vec::new(),
                events: Vec::new(),
            },
        };
        tx.flush()?;
        Ok(tx)
    }

    fn progress(&self, message: impl AsRef<str>) {
        if let Some(progress) = &self.progress {
            progress.println(format!("    · {}", message.as_ref()));
        }
    }

    fn set_progress_bar(&mut self, progress: Option<ProgressBar>) {
        self.progress = progress;
    }

    fn set_status(&mut self, status: TransactionStatus) -> Result<()> {
        tracing::debug!(
            migration_id = %self.manifest.id,
            status = ?status,
            "mv.apply: transaction status transition"
        );
        self.manifest.status = status;
        self.flush()
    }

    fn event(&mut self, event: impl Into<String>) -> Result<()> {
        self.manifest.events.push(event.into());
        self.flush()
    }

    fn snapshot_file(&mut self, path: &Path) -> Result<usize> {
        if let Some((index, _)) = self
            .manifest
            .backups
            .iter()
            .enumerate()
            .find(|(_, record)| record.target == path && matches!(record.kind, BackupKind::File))
        {
            return Ok(index);
        }

        let existed = path.exists();
        let (backup, before_checksum) = if existed {
            let backup = self.dir.join("backups").join(format!(
                "{}-{}",
                self.manifest.backups.len(),
                backup_leaf(path)
            ));
            self.progress(format!(
                "snapshot {} ({})",
                path.display(),
                human_bytes(
                    fs::metadata(path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0)
                )
            ));
            fs::copy(path, &backup).with_context(|| {
                format!(
                    "failed to snapshot {} -> {}",
                    path.display(),
                    backup.display()
                )
            })?;
            (Some(backup), Some(file_checksum(path)?))
        } else {
            (None, None)
        };

        self.manifest.backups.push(BackupRecord {
            kind: BackupKind::File,
            target: path.to_path_buf(),
            backup,
            existed,
            before_checksum,
            after_checksum: None,
        });
        let record = self.manifest.backups.last().expect("backup just pushed");
        tracing::debug!(
            target = %path.display(),
            existed,
            backup = record
                .backup
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            "mv.apply: snapshotted file dependency"
        );
        self.flush()?;
        Ok(self.manifest.backups.len() - 1)
    }

    fn mark_file_after(&mut self, index: usize) -> Result<()> {
        if let Some(record) = self.manifest.backups.get_mut(index) {
            record.after_checksum = if record.target.exists() {
                Some(file_checksum(&record.target)?)
            } else {
                None
            };
        }
        self.flush()
    }

    fn record_rename(&mut self, from: &Path, to: &Path) -> Result<usize> {
        tracing::debug!(
            from = %from.display(),
            to = %to.display(),
            existed = from.exists(),
            "mv.apply: recorded directory move"
        );
        self.manifest.backups.push(BackupRecord {
            kind: BackupKind::Rename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            },
            target: to.to_path_buf(),
            backup: None,
            existed: from.exists(),
            before_checksum: None,
            after_checksum: None,
        });
        self.flush()?;
        Ok(self.manifest.backups.len() - 1)
    }

    fn mark_rename_after(&mut self, index: usize) -> Result<()> {
        if let Some(record) = self.manifest.backups.get_mut(index) {
            record.after_checksum = Some("renamed".to_string());
        }
        self.flush()
    }

    fn rollback(&mut self) -> Result<()> {
        tracing::debug!(
            migration_id = %self.manifest.id,
            backups = self.manifest.backups.len(),
            "mv.apply: rollback starting"
        );
        let mut failures = Vec::new();
        for record in self.manifest.backups.clone().into_iter().rev() {
            if let Err(error) = rollback_record(&record) {
                failures.push(error.to_string());
            }
        }

        self.manifest.status = TransactionStatus::RolledBack;
        if !failures.is_empty() {
            self.manifest
                .events
                .push(format!("rollback failures: {}", failures.join("; ")));
        }
        self.flush()?;

        if failures.is_empty() {
            tracing::debug!(migration_id = %self.manifest.id, "mv.apply: rollback complete");
            Ok(())
        } else {
            bail!("rollback incomplete: {}", failures.join("; "))
        }
    }

    fn flush(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.manifest)?;
        write_bytes_atomic(&self.manifest_path, &bytes)
    }
}

pub fn apply_migration_plan(
    plan: &MigrationDoctorReport,
    options: &MigrationApplyOptions,
) -> Result<MigrationApplyReport> {
    tracing::debug!(
        old_path = %plan.old_path.display(),
        new_path = %plan.new_path.display(),
        dry_run = options.dry_run,
        force = options.force,
        harnesses = plan.harnesses.len(),
        risks = plan.risks.len(),
        "mv.apply: plan execution requested"
    );
    if plan.has_blockers() && !options.force {
        bail!("migration plan has blocker risk(s); rerun doctor or pass force from a higher-level command");
    }

    let edits = plan
        .harnesses
        .iter()
        .flat_map(|harness| harness.edits.iter())
        .collect::<Vec<_>>();
    let mut report = MigrationApplyReport::new(options, edits.len());
    tracing::debug!(edits = edits.len(), "mv.apply: classifying migration edits");

    for edit in &edits {
        match edit.capability {
            ApplyCapability::ApplyReady if recovery_is_executor_owned(edit.recovery) => {
                report.edits_apply_ready += 1;
            }
            ApplyCapability::PreserveOnly => {
                report.skipped.push(format!(
                    "{}:{} is preserve-only",
                    edit.harness.slug(),
                    edit.action
                ));
            }
            ApplyCapability::ApplyReady => report.blockers.push(format!(
                "{}:{} declares apply-ready but recovery class {:?} is not executable yet",
                edit.harness.slug(),
                edit.action,
                edit.recovery
            )),
            ApplyCapability::DoctorOnly | ApplyCapability::Unsupported => {
                report.blockers.push(format!(
                    "{}:{} is {:?}; doctor can show it, but executor will not mutate it",
                    edit.harness.slug(),
                    edit.action,
                    edit.capability
                ))
            }
        }
    }
    tracing::debug!(
        edits_seen = report.edits_seen,
        edits_apply_ready = report.edits_apply_ready,
        skipped = report.skipped.len(),
        blockers = report.blockers.len(),
        "mv.apply: edit classification complete"
    );

    if report.has_blockers() && !options.force {
        bail!("{}", report.blockers.join("\n"));
    }
    if options.dry_run {
        tracing::debug!(
            edits_apply_ready = report.edits_apply_ready,
            "mv.apply: dry run complete; no mutation performed"
        );
        report.applied.push(format!(
            "would apply {} executor-owned edit(s)",
            report.edits_apply_ready
        ));
        return Ok(report);
    }
    if report.edits_apply_ready == 0 {
        tracing::debug!("mv.apply: no executor-owned edits to apply");
        return Ok(report);
    }

    let mut tx = MigrationTransaction::start(plan, options)?;
    report.manifest_path = Some(tx.manifest_path.clone());
    tx.set_status(TransactionStatus::Applying)?;
    let apply_progress = migration_progress_bar(options.print_progress, report.edits_apply_ready);
    tx.set_progress_bar(apply_progress.clone());
    progress_print(
        &apply_progress,
        format!("babel mv manifest {}", tx.manifest_path.display()),
    );

    let apply_result = (|| -> Result<()> {
        let mut mutated_edits = Vec::new();
        for (index, edit) in edits.iter().enumerate() {
            if edit.capability == ApplyCapability::ApplyReady
                && recovery_is_executor_owned(edit.recovery)
            {
                tracing::debug!(
                    harness = %edit.harness.slug(),
                    action = %edit.action,
                    kind = edit_kind_label(&edit.kind),
                    target = %edit.target(),
                    recovery = ?edit.recovery,
                    "mv.apply: applying edit"
                );
                let applied_before = report.applied.len();
                let skipped_before = report.skipped.len();
                let blockers_before = report.blockers.len();
                progress_set_message(
                    &apply_progress,
                    format!("{}:{}", edit.harness.slug(), edit.action),
                );
                progress_print(
                    &apply_progress,
                    format!(
                        "  → {}:{} {} {}",
                        edit.harness.slug(),
                        edit.action,
                        edit_kind_label(&edit.kind),
                        edit.target()
                    ),
                );
                let mutated = apply_edit(edit, &mut tx, &mut report)?;
                for line in &report.applied[applied_before..] {
                    progress_print(&apply_progress, format!("    ✓ {line}"));
                }
                for line in &report.skipped[skipped_before..] {
                    progress_print(&apply_progress, format!("    - {line}"));
                }
                for line in &report.blockers[blockers_before..] {
                    progress_print(&apply_progress, format!("    ! {line}"));
                }
                progress_inc(&apply_progress, 1);
                if report.blockers.len() > blockers_before {
                    bail!("{}", report.blockers[blockers_before..].join("\n"));
                }
                if mutated {
                    mutated_edits.push(index);
                }
                tracing::debug!(
                    harness = %edit.harness.slug(),
                    action = %edit.action,
                    mutated,
                    "mv.apply: edit applied"
                );
            }
        }
        progress_finish(&apply_progress, "apply complete");

        tx.set_status(TransactionStatus::Verifying)?;
        let verify_progress = migration_progress_bar(options.print_progress, mutated_edits.len());
        tx.set_progress_bar(verify_progress.clone());
        for index in mutated_edits {
            let edit = edits[index];
            tracing::debug!(
                harness = %edit.harness.slug(),
                action = %edit.action,
                kind = edit_kind_label(&edit.kind),
                target = %edit.target(),
                "mv.apply: verifying edit"
            );
            verify_edit(edit)?;
            report
                .verified
                .push(format!("{}:{}", edit.harness.slug(), edit.action));
            progress_set_message(
                &verify_progress,
                format!("{}:{}", edit.harness.slug(), edit.action),
            );
            progress_print(
                &verify_progress,
                format!("    ✓ {}:{}", edit.harness.slug(), edit.action),
            );
            progress_inc(&verify_progress, 1);
            tracing::debug!(
                harness = %edit.harness.slug(),
                action = %edit.action,
                "mv.apply: edit verified"
            );
        }
        progress_finish(&verify_progress, "verify complete");
        tx.set_progress_bar(None);
        Ok(())
    })();

    if let Err(error) = apply_result {
        progress_abandon(&apply_progress);
        tracing::debug!(error = %error, "mv.apply: apply failed; beginning rollback");
        tx.set_status(TransactionStatus::Failed)?;
        let rollback_result = tx.rollback();
        report.rolled_back = rollback_result.is_ok();
        if let Err(rollback_error) = rollback_result {
            bail!("{error}; rollback failed: {rollback_error}");
        }
        bail!("{error}; rolled back session-owned migration state");
    }

    tx.event("apply verified")?;
    tx.set_status(TransactionStatus::Complete)?;
    tracing::debug!(
        applied = report.applied.len(),
        verified = report.verified.len(),
        manifest = report
            .manifest_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        "mv.apply: transaction complete"
    );
    Ok(report)
}

fn recovery_is_executor_owned(recovery: RecoveryClass) -> bool {
    matches!(
        recovery,
        RecoveryClass::OwnedFile
            | RecoveryClass::OwnedDir
            | RecoveryClass::SessionDependencyFile
            | RecoveryClass::SessionDependencyDir
    )
}

fn edit_kind_label(kind: &MigrationEditKind) -> &'static str {
    match kind {
        MigrationEditKind::RenamePath { .. } => "rename_path",
        MigrationEditKind::RewriteJsonlField { .. } => "rewrite_jsonl_field",
        MigrationEditKind::RewriteTomlTableKey { .. } => "rewrite_toml_table_key",
        MigrationEditKind::RewriteTextRefs { .. } => "rewrite_text_refs",
        MigrationEditKind::RewriteSqliteTextColumn { .. } => "rewrite_sqlite_text_column",
        MigrationEditKind::PreserveSessionKeyedFiles { .. } => "preserve_session_keyed_files",
        MigrationEditKind::PreserveProjectLocalHistory { .. } => "preserve_project_local_history",
    }
}

fn migration_progress_bar(enabled: bool, total: usize) -> Option<ProgressBar> {
    if !enabled {
        return None;
    }
    let progress = ProgressBar::new(total as u64);
    let style = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:28.cyan/blue}] {pos}/{len} {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("#>-");
    progress.set_style(style);
    Some(progress)
}

fn progress_print(progress: &Option<ProgressBar>, message: impl Into<String>) {
    if let Some(progress) = progress {
        progress.println(message.into());
    }
}

fn progress_set_message(progress: &Option<ProgressBar>, message: impl Into<String>) {
    if let Some(progress) = progress {
        progress.set_message(message.into());
    }
}

fn progress_inc(progress: &Option<ProgressBar>, delta: u64) {
    if let Some(progress) = progress {
        progress.inc(delta);
    }
}

fn progress_finish(progress: &Option<ProgressBar>, message: &'static str) {
    if let Some(progress) = progress {
        progress.finish_with_message(message);
    }
}

fn progress_abandon(progress: &Option<ProgressBar>) {
    if let Some(progress) = progress {
        progress.abandon();
    }
}

fn apply_edit(
    edit: &MigrationEdit,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
) -> Result<bool> {
    match &edit.kind {
        MigrationEditKind::RenamePath { from, to, .. } => apply_rename_path(
            from,
            to,
            tx,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::RewriteJsonlField {
            path,
            files,
            selector,
            from,
            to,
            ..
        } => apply_jsonl_rewrite(
            path,
            files,
            selector,
            from,
            to,
            tx,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::RewriteTomlTableKey {
            path,
            table,
            from_key,
            to_key,
            ..
        } => apply_toml_table_key_rewrite(
            path,
            table,
            from_key,
            to_key,
            tx,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::RewriteTextRefs {
            target,
            files,
            from,
            to,
            ..
        } => apply_text_ref_rewrite(
            target,
            files,
            from,
            to,
            tx,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::RewriteSqliteTextColumn {
            path,
            table,
            column,
            from,
            to,
            ..
        } => apply_sqlite_text_column_rewrite(
            path,
            table,
            column,
            from,
            to,
            tx,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::PreserveSessionKeyedFiles { root, .. } => {
            report
                .skipped
                .push(format!("{} preserves {}", edit.action, root.display()));
            Ok(false)
        }
        MigrationEditKind::PreserveProjectLocalHistory { target, .. } => {
            report
                .skipped
                .push(format!("{} preserves {}", edit.action, target));
            Ok(false)
        }
    }
}

fn apply_rename_path(
    from: &Path,
    to: &Path,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<bool> {
    tracing::debug!(
        label = %label,
        from = %from.display(),
        to = %to.display(),
        source_exists = from.exists(),
        dest_exists = to.exists(),
        "mv.apply: rename transition"
    );
    if !from.exists() && to.exists() {
        report.skipped.push(format!(
            "{label}: already applied; source missing and destination exists: {} -> {}",
            from.display(),
            to.display()
        ));
        return Ok(false);
    }
    if !from.exists() {
        report.blockers.push(format!(
            "{label}: source does not exist: {}",
            from.display()
        ));
        return Ok(false);
    }
    if to.exists() {
        report
            .blockers
            .push(format!("{label}: destination exists: {}", to.display()));
        return Ok(false);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let record = tx.record_rename(from, to)?;
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} -> {}", from.display(), to.display()))?;
    tx.mark_rename_after(record)?;
    report.applied.push(format!(
        "{label}: renamed {} -> {}",
        from.display(),
        to.display()
    ));
    Ok(true)
}

fn apply_jsonl_rewrite(
    path: &Path,
    exact_files: &[PathBuf],
    selector: &str,
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<bool> {
    tracing::debug!(
        label = %label,
        target = %path.display(),
        exact_files = exact_files.len(),
        selector,
        "mv.apply: expanding JSONL rewrite targets"
    );
    let files = jsonl_targets(path, exact_files)?;
    tracing::debug!(
        label = %label,
        target = %path.display(),
        files = files.len(),
        "mv.apply: JSONL rewrite targets resolved"
    );
    if files.is_empty() {
        report.blockers.push(format!(
            "{label}: no JSONL targets under {}",
            path.display()
        ));
        return Ok(false);
    }

    let total = files.len();
    let mut changed = 0;
    for (index, file) in files.into_iter().enumerate() {
        let size = file_size(&file)?;
        if total > 1 || size >= 16 * 1024 * 1024 {
            tx.progress(format!(
                "jsonl {}/{} {} ({})",
                index + 1,
                total,
                file.display(),
                human_bytes(size)
            ));
        }
        changed += rewrite_jsonl_file(&file, selector, from, to, tx)
            .with_context(|| format!("failed to rewrite {}", file.display()))?;
    }

    if changed == 0 {
        report
            .skipped
            .push(format!("{label}: no JSONL records matched"));
        return Ok(false);
    }

    report
        .applied
        .push(format!("{label}: rewrote {changed} JSONL record(s)"));
    tracing::debug!(label = %label, changed, "mv.apply: JSONL rewrite complete");
    Ok(true)
}

fn apply_toml_table_key_rewrite(
    path: &Path,
    table: &str,
    from_key: &str,
    to_key: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<bool> {
    tracing::debug!(
        label = %label,
        path = %path.display(),
        table,
        from_key,
        to_key,
        "mv.apply: TOML key rewrite starting"
    );
    if !path.exists() {
        report
            .blockers
            .push(format!("{label}: TOML file missing: {}", path.display()));
        return Ok(false);
    }

    let content = fs::read_to_string(path)?;
    let old_header = format!("[{table}.\"{from_key}\"]");
    let new_header = format!("[{table}.\"{to_key}\"]");
    if !content.contains(&old_header) {
        report.skipped.push(format!(
            "{label}: no TOML table key matched in {}",
            path.display()
        ));
        return Ok(false);
    }
    let updated = content.replace(&old_header, &new_header);
    write_file_atomic_tx(path, updated.as_bytes(), tx)?;
    report.applied.push(format!(
        "{label}: rewrote TOML table key in {}",
        path.display()
    ));
    tracing::debug!(label = %label, path = %path.display(), "mv.apply: TOML key rewrite complete");
    Ok(true)
}

fn apply_text_ref_rewrite(
    target: &str,
    exact_files: &[PathBuf],
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<bool> {
    tracing::debug!(
        label = %label,
        target,
        exact_files = exact_files.len(),
        source_needle_len = from.len(),
        dest_needle_len = to.len(),
        "mv.apply: text rewrite starting"
    );
    if from.is_empty() {
        report
            .blockers
            .push(format!("{label}: text rewrite has no source needle"));
        return Ok(false);
    }

    let files = if exact_files.is_empty() {
        let path = PathBuf::from(target);
        tracing::debug!(
            label = %label,
            target = %path.display(),
            "mv.apply: expanding broad text rewrite target"
        );
        if !path.exists() {
            report.blockers.push(format!(
                "{label}: text rewrite target is not a concrete path: {target}"
            ));
            return Ok(false);
        }
        text_targets(&path)?
    } else {
        tracing::debug!(
            label = %label,
            exact_files = exact_files.len(),
            sample = %exact_files
                .iter()
                .take(3)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            "mv.apply: using pre-scanned text rewrite files"
        );
        let mut files = Vec::new();
        for file in exact_files {
            if !file.exists() {
                report.blockers.push(format!(
                    "{label}: text rewrite file missing: {}",
                    file.display()
                ));
                continue;
            }
            files.push(file.clone());
        }
        files
    };
    let mut changed = 0;
    for file in files {
        let content = fs::read_to_string(&file)?;
        if !content.contains(from) {
            continue;
        }
        changed += 1;
        let updated = content.replace(from, to);
        write_file_atomic_tx(&file, updated.as_bytes(), tx)?;
    }

    if changed == 0 {
        report
            .skipped
            .push(format!("{label}: no text files matched"));
        return Ok(false);
    }

    report
        .applied
        .push(format!("{label}: rewrote {changed} text file(s)"));
    tracing::debug!(
        label = %label,
        changed,
        "mv.apply: text rewrite complete"
    );
    Ok(true)
}

fn apply_sqlite_text_column_rewrite(
    path: &Path,
    table: &str,
    column: &str,
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<bool> {
    tracing::debug!(
        label = %label,
        path = %path.display(),
        table,
        column,
        "mv.apply: SQLite text column rewrite starting"
    );
    if !path.exists() {
        report.blockers.push(format!(
            "{label}: SQLite database missing: {}",
            path.display()
        ));
        return Ok(false);
    }

    let backup_indices = snapshot_sqlite_family(path, tx)?;
    let table = quote_sql_ident(table)?;
    let column = quote_sql_ident(column)?;
    let sql = format!(
        "UPDATE {table} SET {column} = replace({column}, ?1, ?2) WHERE instr({column}, ?1) > 0"
    );

    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open SQLite database {}", path.display()))?;
    let changed = {
        let tx_sql = conn.transaction()?;
        let changed = tx_sql.execute(&sql, params![from, to])?;
        tx_sql.commit()?;
        changed
    };
    drop(conn);

    for index in backup_indices {
        tx.mark_file_after(index)?;
    }
    if changed == 0 {
        report.skipped.push(format!(
            "{label}: no SQLite rows matched in {}",
            path.display()
        ));
        return Ok(false);
    }
    report.applied.push(format!(
        "{label}: rewrote {changed} SQLite row(s) in {}",
        path.display()
    ));
    tracing::debug!(
        label = %label,
        path = %path.display(),
        changed,
        "mv.apply: SQLite text column rewrite complete"
    );
    Ok(true)
}

fn verify_edit(edit: &MigrationEdit) -> Result<()> {
    tracing::debug!(
        harness = %edit.harness.slug(),
        action = %edit.action,
        kind = edit_kind_label(&edit.kind),
        "mv.apply: verification dispatch"
    );
    match &edit.verification {
        VerificationSpec::PathMoved { from, to } => {
            if from.exists() || !to.exists() {
                bail!(
                    "path move verification failed: {} -> {}",
                    from.display(),
                    to.display()
                );
            }
        }
        VerificationSpec::JsonlFieldRewritten {
            path,
            files,
            selector,
            from,
            to,
            expected_count,
        } => {
            let old_count = count_jsonl_matches(path, files, selector, from)?;
            let new_count = count_jsonl_matches(path, files, selector, to)?;
            if old_count > 0 || new_count < *expected_count {
                bail!(
                    "JSONL verification failed for {}: old={}, new={}, expected_new>={}",
                    path.display(),
                    old_count,
                    new_count,
                    expected_count
                );
            }
        }
        VerificationSpec::TomlKeyMoved {
            path,
            table,
            from_key,
            to_key,
        } => {
            let content = fs::read_to_string(path)?;
            let old_header = format!("[{table}.\"{from_key}\"]");
            let new_header = format!("[{table}.\"{to_key}\"]");
            if content.contains(&old_header) || !content.contains(&new_header) {
                bail!("TOML verification failed for {}", path.display());
            }
        }
        VerificationSpec::TextRefsReduced {
            target,
            files,
            from,
            ..
        } => {
            tracing::debug!(
                harness = %edit.harness.slug(),
                action = %edit.action,
                target,
                exact_files = files.len(),
                "mv.apply: text reference verification starting"
            );
            let targets = if files.is_empty() {
                let path = PathBuf::from(target);
                tracing::debug!(
                    harness = %edit.harness.slug(),
                    action = %edit.action,
                    target = %path.display(),
                    "mv.apply: expanding broad text verification target"
                );
                text_targets(&path)?
            } else {
                files.clone()
            };
            let old_refs = targets
                .into_iter()
                .filter_map(|path| fs::read_to_string(path).ok())
                .filter(|content| content.contains(from))
                .count();
            if old_refs > 0 {
                bail!("text reference verification failed for {target}: old refs remain");
            }
            tracing::debug!(
                harness = %edit.harness.slug(),
                action = %edit.action,
                old_refs,
                "mv.apply: text reference verification complete"
            );
        }
        VerificationSpec::SqliteTextColumnRewritten {
            path,
            table,
            column,
            from,
            to,
            expected_count,
        } => {
            let old_refs = count_sqlite_text_column_refs(path, table, column, from)?;
            let new_refs = count_sqlite_text_column_refs(path, table, column, to)?;
            if old_refs > 0 || new_refs < *expected_count {
                bail!(
                    "SQLite verification failed for {} {table}.{column}: old={}, new={}, expected_new>={}",
                    path.display(),
                    old_refs,
                    new_refs,
                    expected_count
                );
            }
        }
        VerificationSpec::SessionCountPreserved { .. } | VerificationSpec::PreserveOnly => {}
    }
    Ok(())
}

fn snapshot_sqlite_family(path: &Path, tx: &mut MigrationTransaction) -> Result<Vec<usize>> {
    let mut indices = vec![tx.snapshot_file(path)?];
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.exists() {
            indices.push(tx.snapshot_file(&sidecar)?);
        }
    }
    Ok(indices)
}

fn quote_sql_ident(value: &str) -> Result<String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        bail!("unsafe SQLite identifier: {value}");
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

fn count_sqlite_text_column_refs(
    path: &Path,
    table: &str,
    column: &str,
    needle: &str,
) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let table = quote_sql_ident(table)?;
    let column = quote_sql_ident(column)?;
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE instr({column}, ?1) > 0");
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(conn.query_row(&sql, params![needle], |row| row.get(0))?)
}

fn jsonl_targets(path: &Path, exact_files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !exact_files.is_empty() {
        tracing::debug!(
            target = %path.display(),
            exact_files = exact_files.len(),
            "mv.apply: JSONL target uses pre-scanned files"
        );
        return Ok(exact_files.to_vec());
    }
    if path.is_file() {
        tracing::debug!(target = %path.display(), "mv.apply: JSONL target is a file");
        return Ok(vec![path.to_path_buf()]);
    }
    text_targets(path).map(|files| {
        files
            .into_iter()
            .filter(|file| file.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect()
    })
}

fn text_targets(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if path.is_file() && is_probably_text_migration_file(path) {
        tracing::debug!(target = %path.display(), "mv.apply: text target is a file");
        files.push(path.to_path_buf());
        return Ok(files);
    }
    if !path.is_dir() {
        tracing::debug!(
            target = %path.display(),
            "mv.apply: text target is neither file nor directory"
        );
        return Ok(files);
    }

    tracing::debug!(
        target = %path.display(),
        "mv.apply: text target tree scan starting"
    );
    let mut stack = vec![path.to_path_buf()];
    let mut dirs_scanned = 0usize;
    while let Some(path) = stack.pop() {
        dirs_scanned += 1;
        for entry in fs::read_dir(&path)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() && is_probably_text_migration_file(&path) {
                files.push(path);
            }
        }
    }
    tracing::debug!(
        target = %path.display(),
        dirs_scanned,
        files = files.len(),
        "mv.apply: text target tree scan complete"
    );
    Ok(files)
}

fn is_probably_text_migration_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "json"
                | "jsonl"
                | "toml"
                | "yaml"
                | "yml"
                | "md"
                | "txt"
                | "log"
                | "history"
                | "tsx"
                | "ts"
                | "jsx"
                | "js"
                | "py"
                | "rs"
                | "sh"
                | "fish"
                | "zsh"
                | "bash"
        )
    ) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "config" | "history" | "settings" | "state" | "taskHistory"
            )
        })
}

fn rewrite_jsonl_file(
    path: &Path,
    selector: &str,
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
) -> Result<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let tmp = atomic_temp_path(path)?;
    let mut writer = BufWriter::new(
        fs::File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?,
    );
    let mut changed = 0;

    for line in reader.lines() {
        let line = line?;
        let (updated, did_change) = rewrite_jsonl_line(&line, selector, from, to)?;
        changed += usize::from(did_change);
        writer.write_all(updated.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer
        .get_ref()
        .sync_all()
        .with_context(|| format!("failed to sync {}", tmp.display()))?;
    drop(writer);

    if changed > 0 {
        promote_rewrite_temp_tx(path, &tmp, tx)?;
    } else if tmp.exists() {
        fs::remove_file(&tmp).with_context(|| format!("failed to remove {}", tmp.display()))?;
    }
    Ok(changed)
}

fn count_jsonl_matches(
    path: &Path,
    exact_files: &[PathBuf],
    selector: &str,
    needle: &str,
) -> Result<usize> {
    let mut count = 0;
    for file in jsonl_targets(path, exact_files)? {
        let reader = BufReader::new(fs::File::open(&file)?);
        for line in reader.lines() {
            let line = line?;
            if jsonl_line_matches(&line, selector, needle)? {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn jsonl_line_matches(line: &str, selector: &str, needle: &str) -> Result<bool> {
    if selector == "line containing source path" {
        return Ok(line.contains(needle));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok(false);
    };
    let text = match selector {
        "$.project" => value.get("project").and_then(|slot| slot.as_str()),
        "$.payload.cwd where $.type == \"session_meta\"" => {
            if value.get("type").and_then(|kind| kind.as_str()) == Some("session_meta") {
                value
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(|slot| slot.as_str())
            } else {
                None
            }
        }
        "$.payload.cwd where $.type == \"turn_context\"" => {
            if value.get("type").and_then(|kind| kind.as_str()) == Some("turn_context") {
                value
                    .get("payload")
                    .and_then(|payload| payload.get("cwd"))
                    .and_then(|slot| slot.as_str())
            } else {
                None
            }
        }
        other => bail!("unsupported JSONL selector: {other}"),
    };
    Ok(text.is_some_and(|text| text == needle || text.starts_with(&format!("{needle}/"))))
}

fn rewrite_jsonl_line(line: &str, selector: &str, from: &str, to: &str) -> Result<(String, bool)> {
    if selector == "line containing source path" {
        if line.contains(from) {
            return Ok((line.replace(from, to), true));
        }
        return Ok((line.to_string(), false));
    }

    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Ok((line.to_string(), false));
    };

    let changed = match selector {
        "$.project" => replace_json_string_field(&mut value, &["project"], from, to),
        "$.payload.cwd where $.type == \"session_meta\"" => {
            if value.get("type").and_then(|kind| kind.as_str()) == Some("session_meta") {
                replace_json_string_field(&mut value, &["payload", "cwd"], from, to)
            } else {
                false
            }
        }
        "$.payload.cwd where $.type == \"turn_context\"" => {
            if value.get("type").and_then(|kind| kind.as_str()) == Some("turn_context") {
                replace_json_string_field(&mut value, &["payload", "cwd"], from, to)
            } else {
                false
            }
        }
        other => bail!("unsupported JSONL selector: {other}"),
    };

    if !changed {
        return Ok((line.to_string(), false));
    }
    Ok((serde_json::to_string(&value)?, true))
}

fn replace_json_string_field(
    value: &mut serde_json::Value,
    path: &[&str],
    from: &str,
    to: &str,
) -> bool {
    let mut current = value;
    for key in &path[..path.len().saturating_sub(1)] {
        let Some(next) = current.get_mut(*key) else {
            return false;
        };
        current = next;
    }
    let Some(last) = path.last() else {
        return false;
    };
    let Some(slot) = current.get_mut(*last) else {
        return false;
    };
    let Some(text) = slot.as_str() else {
        return false;
    };
    if text != from && !text.starts_with(&format!("{from}/")) {
        return false;
    }
    *slot = serde_json::Value::String(text.replacen(from, to, 1));
    true
}

fn write_file_atomic_tx(path: &Path, content: &[u8], tx: &mut MigrationTransaction) -> Result<()> {
    let original = FileMetadataStamp::read(path)?;
    let snapshot = tx.snapshot_file(path)?;
    write_bytes_atomic(path, content)?;
    original.restore(path)?;
    tx.mark_file_after(snapshot)
}

fn promote_rewrite_temp_tx(path: &Path, tmp: &Path, tx: &mut MigrationTransaction) -> Result<()> {
    let original = FileMetadataStamp::read(path)?;
    let snapshot = tx.snapshot_file(path)?;
    fs::rename(tmp, path).with_context(|| {
        format!(
            "failed to promote rewrite {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    original.restore(path)?;
    tx.mark_file_after(snapshot)
}

struct FileMetadataStamp {
    permissions: Option<fs::Permissions>,
    accessed: Option<FileTime>,
    modified: Option<FileTime>,
}

impl FileMetadataStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    permissions: None,
                    accessed: None,
                    modified: None,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to stat {}", path.display()));
            }
        };

        Ok(Self {
            permissions: Some(metadata.permissions()),
            accessed: Some(FileTime::from_last_access_time(&metadata)),
            modified: Some(FileTime::from_last_modification_time(&metadata)),
        })
    }

    fn restore(&self, path: &Path) -> Result<()> {
        if let Some(permissions) = &self.permissions {
            fs::set_permissions(path, permissions.clone())
                .with_context(|| format!("failed to restore permissions on {}", path.display()))?;
        }
        if let (Some(accessed), Some(modified)) = (self.accessed, self.modified) {
            set_file_times(path, accessed, modified)
                .with_context(|| format!("failed to restore timestamps on {}", path.display()))?;
            tracing::debug!(
                target = %path.display(),
                "mv.apply: restored file metadata after content rewrite"
            );
        }
        Ok(())
    }
}

fn write_bytes_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path_parent(path)?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let tmp = atomic_temp_path(path)?;
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to promote rewrite {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn atomic_temp_path(path: &Path) -> Result<PathBuf> {
    let parent = path_parent(path)?;
    Ok(parent.join(format!(
        ".{}.babel-mv-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    )))
}

fn path_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len())
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn rollback_record(record: &BackupRecord) -> Result<()> {
    match &record.kind {
        BackupKind::File => rollback_file(record),
        BackupKind::Rename { from, to } => {
            if from.exists() {
                bail!(
                    "cannot rollback rename {} -> {}; source already exists",
                    from.display(),
                    to.display()
                );
            }
            if to.exists() {
                fs::rename(to, from).with_context(|| {
                    format!(
                        "failed to rollback rename {} -> {}",
                        to.display(),
                        from.display()
                    )
                })?;
            }
            Ok(())
        }
    }
}

fn rollback_file(record: &BackupRecord) -> Result<()> {
    if let Some(after_checksum) = &record.after_checksum {
        if record.target.exists() && file_checksum(&record.target)? != *after_checksum {
            bail!(
                "refusing to rollback externally changed file {}",
                record.target.display()
            );
        }
    }

    if record.existed {
        let backup = record
            .backup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing backup for {}", record.target.display()))?;
        fs::copy(backup, &record.target).with_context(|| {
            format!(
                "failed to restore {} from {}",
                record.target.display(),
                backup.display()
            )
        })?;
    } else if record.target.exists() {
        fs::remove_file(&record.target)
            .with_context(|| format!("failed to remove {}", record.target.display()))?;
    }
    Ok(())
}

fn default_transaction_root() -> PathBuf {
    dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("babel/migrations")
}

fn migration_id(old_path: &Path, new_path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    old_path.hash(&mut hasher);
    new_path.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let now = chrono::Utc::now();
    format!("{}-{:016x}", now.format("%Y%m%dT%H%M%SZ"), hasher.finish())
}

fn backup_leaf(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| sanitize_for_filename(name))
        .unwrap_or_else(|| "state".to_string())
}

fn sanitize_for_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn file_checksum(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("fnv64:{hash:016x}"))
}
