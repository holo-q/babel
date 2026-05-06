use anyhow::Result;

use crate::native_sessions::shared::{
    all_jsonl_sessions, env_path, home_path, push_existing_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct AntigravityScanner;

impl NativeSessionScanner for AntigravityScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Antigravity
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        let mut roots = Vec::new();
        push_existing_path(&mut roots, env_path("ANTIGRAVITY_HOME"));
        push_existing_path(
            &mut roots,
            env_path("GEMINI_CLI_HOME").map(|path| path.join("antigravity")),
        );
        push_existing_path(&mut roots, home_path(&[".gemini", "antigravity"]));
        Ok(all_jsonl_sessions(AgentKind::Antigravity, roots, 6))
    }
}
