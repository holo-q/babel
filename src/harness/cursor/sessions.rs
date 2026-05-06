use anyhow::Result;

use crate::native_sessions::shared::{
    all_jsonl_sessions, home_path, push_existing_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct CursorScanner;

impl NativeSessionScanner for CursorScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Cursor
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        let mut roots = Vec::new();
        push_existing_path(&mut roots, home_path(&[".cursor", "projects"]));
        Ok(all_jsonl_sessions(AgentKind::Cursor, roots, 5))
    }
}
