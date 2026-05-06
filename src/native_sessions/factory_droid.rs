use anyhow::Result;

use crate::native_sessions::shared::{
    all_jsonl_sessions, home_path, push_existing_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct FactoryDroidScanner;

impl NativeSessionScanner for FactoryDroidScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::FactoryDroid
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        let mut roots = Vec::new();
        push_existing_path(&mut roots, home_path(&[".factory", "projects"]));
        push_existing_path(&mut roots, home_path(&[".factory", "sessions"]));
        Ok(all_jsonl_sessions(AgentKind::FactoryDroid, roots, 3))
    }
}
