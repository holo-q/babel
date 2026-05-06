use anyhow::Result;

use crate::native_sessions::shared::{scan_cline_family, NativeSession};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct ClineScanner;

impl NativeSessionScanner for ClineScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Cline
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan_cline_family(
            AgentKind::Cline,
            &["CLINE_STORAGE_PATH", "CONTINUES_CLINE_STORAGE_PATH"],
            &["saoudrizwan.claude-dev"],
        )
    }
}
