//! Project migration command.
//!
//! Mutation is intentionally disabled here. `babel mv --doctor` is the current
//! supported surface: it inspects native harness storage and reports the
//! migration graph without moving files.

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

pub fn resolve_destination(source: &Path, dest: &Path) -> PathBuf {
    if dest.is_dir() {
        if let Some(name) = source.file_name() {
            return dest.join(name);
        }
    }
    dest.to_path_buf()
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
    let source = expand_tilde(&source);
    let dest = resolve_destination(&source, &expand_tilde(&dest));
    bail!(
        "babel mv apply is not available yet.\n\
         Run `babel mv --doctor {} {}` to inspect the migration plan. No files or harness state were changed.",
        source.display(),
        dest.display()
    )
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
