use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::{MigrationDoctorReport, MigrationEdit, MigrationEditKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationApplyOptions {
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationApplyReport {
    pub dry_run: bool,
    pub edits_seen: usize,
    pub edits_apply_ready: usize,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub blockers: Vec<String>,
}

impl MigrationApplyReport {
    fn new(options: &MigrationApplyOptions, edits_seen: usize) -> Self {
        Self {
            dry_run: options.dry_run,
            edits_seen,
            edits_apply_ready: 0,
            applied: Vec::new(),
            skipped: Vec::new(),
            blockers: Vec::new(),
        }
    }

    pub fn has_blockers(&self) -> bool {
        !self.blockers.is_empty()
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

    for edit in edits {
        if !edit.apply_ready {
            report.blockers.push(format!(
                "{}:{} is not apply-ready; doctor can show it, but executor will not mutate it yet",
                edit.harness.slug(),
                edit.action
            ));
            continue;
        }

        report.edits_apply_ready += 1;
        apply_edit(edit, options, &mut report)?;
    }

    if report.has_blockers() && !options.force {
        bail!("{}", report.blockers.join("\n"));
    }

    Ok(report)
}

fn apply_edit(
    edit: &MigrationEdit,
    options: &MigrationApplyOptions,
    report: &mut MigrationApplyReport,
) -> Result<()> {
    match &edit.kind {
        MigrationEditKind::RenamePath { from, to, .. } => apply_rename_path(
            from,
            to,
            options,
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
            options,
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
            options,
            report,
            format!("{}:{}", edit.harness.slug(), edit.action),
        ),
        MigrationEditKind::RewriteTextRefs {
            target, from, to, ..
        } => apply_text_ref_rewrite(
            target,
            from,
            to,
            options,
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
    options: &MigrationApplyOptions,
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
    if options.dry_run {
        report.applied.push(format!(
            "{label}: would rename {} -> {}",
            from.display(),
            to.display()
        ));
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::rename(from, to)
        .with_context(|| format!("failed to rename {} -> {}", from.display(), to.display()))?;
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
    options: &MigrationApplyOptions,
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
        changed += rewrite_jsonl_file(&file, selector, from, to, options.dry_run)
            .with_context(|| format!("failed to rewrite {}", file.display()))?;
    }

    report.applied.push(if options.dry_run {
        format!("{label}: would rewrite {changed} JSONL record(s)")
    } else {
        format!("{label}: rewrote {changed} JSONL record(s)")
    });
    Ok(())
}

fn apply_toml_table_key_rewrite(
    path: &Path,
    table: &str,
    from_key: &str,
    to_key: &str,
    options: &MigrationApplyOptions,
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
    if !options.dry_run {
        write_file_atomic(path, updated.as_bytes())?;
    }
    report.applied.push(if options.dry_run {
        format!(
            "{label}: would rewrite TOML table key in {}",
            path.display()
        )
    } else {
        format!("{label}: rewrote TOML table key in {}", path.display())
    });
    Ok(())
}

fn apply_text_ref_rewrite(
    target: &str,
    from: &str,
    to: &str,
    options: &MigrationApplyOptions,
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
        if !options.dry_run {
            let updated = content.replace(from, to);
            write_file_atomic(&file, updated.as_bytes())?;
        }
    }

    report.applied.push(if options.dry_run {
        format!("{label}: would rewrite {changed} text file(s)")
    } else {
        format!("{label}: rewrote {changed} text file(s)")
    });
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
    dry_run: bool,
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

    if changed > 0 && !dry_run {
        let mut content = updated_lines.join("\n");
        content.push('\n');
        write_file_atomic(path, content.as_bytes())?;
    }
    Ok(changed)
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

fn write_file_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
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
