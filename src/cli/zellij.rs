//! Zellij integration — sink adapter for babel bridge
//!
//! Delivers BridgeState to the babel-zellij WASM plugin via `zellij pipe`.

use anyhow::Result;

use super::bridge::{BridgeSink, BridgeState};

// =============================================================================
// ZellijSink — pipes JSON state to the babel-zellij plugin
// =============================================================================

pub struct ZellijSink;

impl BridgeSink for ZellijSink {
    fn deliver(&mut self, state: &BridgeState) -> Result<()> {
        let json = serde_json::to_string(state)?;
        let _ = std::process::Command::new("zellij")
            .args(["pipe", "--name", "babel", "--", &json])
            .output();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "zellij"
    }
}

// =============================================================================
// babel zellij-bridge (alias for babel bridge --zellij)
// =============================================================================

pub async fn cmd_zellij_bridge() -> Result<()> {
    super::bridge::run_bridge(Box::new(ZellijSink)).await
}
