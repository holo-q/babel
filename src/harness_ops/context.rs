use std::env;
use std::path::PathBuf;

use anyhow::Result;

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
