//! Project migration command.
//!
//! Mutation is intentionally disabled here. `babel mv --doctor` is the current
//! supported surface: it inspects native harness storage and reports the
//! migration graph without moving files. The old mover only understood Claude
//! storage and used a copy/delete fallback that was not a lossless filesystem
//! move, so keeping it callable would violate the migration safety contract.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use claude_babel::core::BabelCore;

pub fn expand_tilde(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if let Some(stripped) = path_str.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }

    path.to_path_buf()
}

pub async fn cmd_mv(
    _core: &mut BabelCore,
    source: PathBuf,
    dest: PathBuf,
    _dry_run: bool,
    _history_only: bool,
    _anxious: bool,
    _force: bool,
    _json: bool,
) -> Result<()> {
    bail!(
        "babel mv mutation is disabled until the harness migration executor has backup/verify/rollback semantics.\n\
         Run `babel mv --doctor {} {}` to inspect affected native harness storage.",
        source.display(),
        dest.display()
    )
}
