use anyhow::Result;

use crate::native_sessions::shared::{scan_cline_family, NativeSession};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct KiloCodeScanner;

impl NativeSessionScanner for KiloCodeScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::KiloCode
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        scan_cline_family(
            AgentKind::KiloCode,
            &["KILO_CODE_STORAGE_PATH", "CONTINUES_KILO_CODE_STORAGE_PATH"],
            &["kilocode.kilo-code"],
        )
    }
}
