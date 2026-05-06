use anyhow::Result;

use crate::native_sessions::shared::NativeSession;
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct AiderScanner;

impl NativeSessionScanner for AiderScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Aider
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        Ok(Vec::new())
    }
}
