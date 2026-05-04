//! Shared probing/scanning substrate used by every harness adapter.
//!
//! The adapters under `harness_ops::*` repeatedly need the same primitives:
//! cap-bounded recursive text scanning, JSONL line iteration with honest
//! short-circuit semantics, and a uniform SQLite open with the project's
//! standard flag set. Centralising them here lets the adapters stay focused
//! on harness-specific shape while sharing one verified implementation of
//! the cross-cutting concerns.
//!
//! Visibility is intentionally `pub(super)`: these helpers are tooling for the
//! `harness_ops` module tree only, matching the pre-extraction private-at-parent
//! semantics where the originals lived in `harness_ops.rs` and were visible to
//! its child modules but not to the rest of the crate.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::Result;

pub(super) const MAX_SCAN_FILES: usize = 5_000;
pub(super) const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const LARGE_FILE_SAMPLE_BYTES: usize = 512 * 1024;

#[derive(Default)]
pub(super) struct TextScan {
    pub(super) files_scanned: usize,
    pub(super) path_references_found: usize,
    pub(super) truncated: bool,
    pub(super) large_files_sampled: usize,
}

/// Visit successfully-parsed JSON values in a JSONL file, with optional line cap and
/// honest early-exit semantics.
///
/// Wave 9 collapses the `read_session_*` JSONL scanners that all repeated the
/// same incantation: open, wrap in `BufReader`, optionally take the first N
/// lines, trim, `serde_json::from_str`, ignore malformed. Behavior is
/// preserved verbatim:
/// - missing path returns `Ok(None)` so callers can drop their own existence
///   check and a non-existent file never surfaces a spurious IO error;
/// - IO errors on lines that *are* read still propagate via `line?`;
/// - empty / whitespace-only lines are skipped before parsing;
/// - parse failures are silently ignored — JSONL files in the wild interleave
///   junk lines, especially around crashes, and the scanners want signal not
///   strictness;
/// - a `Some(n)` cap is enforced via `Iterator::take(n)`; `None` is unbounded.
///
/// `ControlFlow::Break(R)` short-circuits *honestly*: once the visitor breaks,
/// later lines are never read, so a downstream IO error past the decisive line
/// can never surface. Factory's identity scanner exploits this — the first
/// `session_start` is decisive and lines after it should not influence the
/// outcome at all. Callers that need a prefix transform (e.g., antigravity's
/// legacy chat scanner stripping a binary header) intentionally don't route
/// here; one-off prefix logic does not justify a generic transform parameter.
pub(super) fn for_each_jsonl_value<R>(
    path: &Path,
    max_lines: Option<usize>,
    mut visit: impl FnMut(&serde_json::Value) -> ControlFlow<R>,
) -> Result<Option<R>> {
    if !path.exists() {
        return Ok(None);
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut remaining = max_lines;
    while remaining.map_or(true, |n| n > 0) {
        let Some(line) = lines.next() else {
            break;
        };
        let line = line?;
        if let Some(n) = remaining.as_mut() {
            *n -= 1;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if let ControlFlow::Break(out) = visit(&value) {
            return Ok(Some(out));
        }
    }
    Ok(None)
}

/// Open a SQLite database read-only with the harness adapters' standard flag set.
///
/// Babel Wave 8 deduplicated four near-identical opens spread across the
/// `cursor`, `crush`, `opencode`, `codex`, and `apply` adapters. The flag set
/// (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`) is preserved verbatim — the
/// no-mutex bit matters because adapters never share a `Connection` across
/// threads, and the read-only bit is the contract for *inspection* helpers
/// that must not mutate provider-native state.
pub(super) fn open_sqlite_read_only(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Open a SQLite database read-write with the harness adapters' standard flag set.
///
/// Mirror of [`open_sqlite_read_only`] for the apply path: `apply.rs` performs
/// in-place rewrites of provider DBs inside an outer transaction. The flags
/// (`SQLITE_OPEN_READ_WRITE | SQLITE_OPEN_NO_MUTEX`) match the pre-Wave-8
/// behavior exactly — *no* `SQLITE_OPEN_CREATE`, so a missing DB is still an
/// error rather than a silently-created empty file.
pub(super) fn open_sqlite_read_write(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

pub(super) fn scan_text_refs(root: &Path, needles: &[String]) -> Result<TextScan> {
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

pub(super) fn text_file_contains_any(path: &Path, len: u64, needles: &[String]) -> Result<bool> {
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

pub(super) fn is_probably_text_state_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json") | Some("jsonl") | Some("toml") | Some("txt") | Some("md")
    )
}
