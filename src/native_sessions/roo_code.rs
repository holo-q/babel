use anyhow::Result;

use crate::native_sessions::shared::{scan_cline_family, NativeSession};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct RooCodeScanner;

impl NativeSessionScanner for RooCodeScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::RooCode
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan_cline_family(
            AgentKind::RooCode,
            &[
                "ROO_CODE_STORAGE_PATH",
                "ROO_CLINE_STORAGE_PATH",
                "CONTINUES_ROO_CODE_STORAGE_PATH",
            ],
            &["rooveterinaryinc.roo-cline", "roo-code.roo-cline"],
        )
    }
}
