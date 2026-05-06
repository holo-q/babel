use anyhow::Result;

use crate::native_sessions::shared::{
    all_jsonl_sessions, env_path, home_path, push_existing_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct QwenCodeScanner;

impl NativeSessionScanner for QwenCodeScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::QwenCode
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        let mut roots = Vec::new();
        push_existing_path(&mut roots, env_path("QWEN_RUNTIME_DIR"));
        push_existing_path(&mut roots, env_path("QWEN_HOME"));
        push_existing_path(&mut roots, home_path(&[".qwen"]));
        Ok(all_jsonl_sessions(AgentKind::QwenCode, roots, 5))
    }
}
