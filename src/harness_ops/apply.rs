use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
        fs::create_dir_all(&backups)
            .with_context(|| format!("failed to create {}", backups.display()))?;
        let manifest_path = dir.join("manifest.json");
        let tx = Self {
            dir,
            manifest_path,
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

    fn set_status(&mut self, status: TransactionStatus) -> Result<()> {
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
    if plan.has_blockers() && !options.force {
        bail!("migration plan has blocker risk(s); rerun doctor or pass force from a higher-level command");
    }

    let edits = plan
        .harnesses
        .iter()
        .flat_map(|harness| harness.edits.iter())
        .collect::<Vec<_>>();
    let mut report = MigrationApplyReport::new(options, edits.len());

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

    if report.has_blockers() && !options.force {
        bail!("{}", report.blockers.join("\n"));
    }
    if options.dry_run {
        report.applied.push(format!(
            "would apply {} executor-owned edit(s)",
            report.edits_apply_ready
        ));
        return Ok(report);
    }
    if report.edits_apply_ready == 0 {
        return Ok(report);
    }

    let mut tx = MigrationTransaction::start(plan, options)?;
    report.manifest_path = Some(tx.manifest_path.clone());
    tx.set_status(TransactionStatus::Applying)?;

    let apply_result = (|| -> Result<()> {
        for edit in &edits {
            if edit.capability == ApplyCapability::ApplyReady
                && recovery_is_executor_owned(edit.recovery)
            {
                apply_edit(edit, &mut tx, &mut report)?;
            }
        }

        tx.set_status(TransactionStatus::Verifying)?;
        for edit in &edits {
            if edit.capability == ApplyCapability::ApplyReady
                && recovery_is_executor_owned(edit.recovery)
            {
                verify_edit(edit)?;
                report
                    .verified
                    .push(format!("{}:{}", edit.harness.slug(), edit.action));
            }
        }
        Ok(())
    })();

    if let Err(error) = apply_result {
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

fn apply_edit(
    edit: &MigrationEdit,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
) -> Result<()> {
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
            selector,
            from,
            to,
            ..
        } => apply_jsonl_rewrite(
            path,
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
            target, from, to, ..
        } => apply_text_ref_rewrite(
            target,
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
            Ok(())
        }
        MigrationEditKind::PreserveProjectLocalHistory { target, .. } => {
            report
                .skipped
                .push(format!("{} preserves {}", edit.action, target));
            Ok(())
        }
    }
}

fn apply_rename_path(
    from: &Path,
    to: &Path,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<()> {
    if !from.exists() {
        report.blockers.push(format!(
            "{label}: source does not exist: {}",
            from.display()
        ));
        return Ok(());
    }
    if to.exists() {
        report
            .blockers
            .push(format!("{label}: destination exists: {}", to.display()));
        return Ok(());
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
    Ok(())
}

fn apply_jsonl_rewrite(
    path: &Path,
    selector: &str,
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<()> {
    let files = jsonl_targets(path)?;
    if files.is_empty() {
        report.blockers.push(format!(
            "{label}: no JSONL targets under {}",
            path.display()
        ));
        return Ok(());
    }

    let mut changed = 0;
    for file in files {
        changed += rewrite_jsonl_file(&file, selector, from, to, tx)
            .with_context(|| format!("failed to rewrite {}", file.display()))?;
    }

    report
        .applied
        .push(format!("{label}: rewrote {changed} JSONL record(s)"));
    Ok(())
}

fn apply_toml_table_key_rewrite(
    path: &Path,
    table: &str,
    from_key: &str,
    to_key: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<()> {
    if !path.exists() {
        report
            .blockers
            .push(format!("{label}: TOML file missing: {}", path.display()));
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let old_header = format!("[{table}.\"{from_key}\"]");
    let new_header = format!("[{table}.\"{to_key}\"]");
    if !content.contains(&old_header) {
        report.skipped.push(format!(
            "{label}: no TOML table key matched in {}",
            path.display()
        ));
        return Ok(());
    }
    let updated = content.replace(&old_header, &new_header);
    write_file_atomic_tx(path, updated.as_bytes(), tx)?;
    report.applied.push(format!(
        "{label}: rewrote TOML table key in {}",
        path.display()
    ));
    Ok(())
}

fn apply_text_ref_rewrite(
    target: &str,
    from: &str,
    to: &str,
    tx: &mut MigrationTransaction,
    report: &mut MigrationApplyReport,
    label: String,
) -> Result<()> {
    if from.is_empty() {
        report
            .blockers
            .push(format!("{label}: text rewrite has no source needle"));
        return Ok(());
    }

    let path = PathBuf::from(target);
    if !path.exists() {
        report.blockers.push(format!(
            "{label}: text rewrite target is not a concrete path: {target}"
        ));
        return Ok(());
    }

    let files = text_targets(&path)?;
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

    report
        .applied
        .push(format!("{label}: rewrote {changed} text file(s)"));
    Ok(())
}

fn verify_edit(edit: &MigrationEdit) -> Result<()> {
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
            selector,
            from,
            to,
            expected_count,
        } => {
            let old_count = count_jsonl_matches(path, selector, from)?;
            let new_count = count_jsonl_matches(path, selector, to)?;
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
        VerificationSpec::TextRefsReduced { target, from, .. } => {
            let path = PathBuf::from(target);
            let old_refs = text_targets(&path)?
                .into_iter()
                .filter_map(|path| fs::read_to_string(path).ok())
                .filter(|content| content.contains(from))
                .count();
            if old_refs > 0 {
                bail!("text reference verification failed for {target}: old refs remain");
            }
        }
        VerificationSpec::SessionCountPreserved { .. } | VerificationSpec::PreserveOnly => {}
    }
    Ok(())
}

fn jsonl_targets(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
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
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(files);
    }
    if !path.is_dir() {
        return Ok(files);
    }

    let mut stack = vec![path.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
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
    let mut updated_lines = Vec::new();
    let mut changed = 0;

    for line in reader.lines() {
        let line = line?;
        let (updated, did_change) = rewrite_jsonl_line(&line, selector, from, to)?;
        changed += usize::from(did_change);
        updated_lines.push(updated);
    }

    if changed > 0 {
        let mut content = updated_lines.join("\n");
        content.push('\n');
        write_file_atomic_tx(path, content.as_bytes(), tx)?;
    }
    Ok(changed)
}

fn count_jsonl_matches(path: &Path, selector: &str, needle: &str) -> Result<usize> {
    let mut count = 0;
    for file in jsonl_targets(path)? {
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
    let snapshot = tx.snapshot_file(path)?;
    write_bytes_atomic(path, content)?;
    tx.mark_file_after(snapshot)
}

fn write_bytes_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.babel-mv-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
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
