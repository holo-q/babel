//! Shared domain model.
//!
//! This module is the refactor boundary for facts that are true across daemon,
//! core, CLI, IPC, and panel-facing code. Runtime modules may add operations,
//! but identity and activity vocabulary should live here so the rest of Babel
//! speaks one language.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::AgentKind;

/// Canonical live pane identity.
///
/// Kitty pane ids are unique only inside one kitty remote-control socket. A
/// live pane address must therefore carry both the socket and the pane id. This
/// is intentionally not a durable session identity: sockets are runtime
/// coordinates, while sessions are harness-owned history.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneAddr {
    /// Socket path, e.g. `unix:/run/user/1000/kitty.sock-12345`.
    pub socket: String,
    /// Pane id inside that kitty instance.
    pub id: u64,
}

impl PaneAddr {
    pub fn new(socket: impl Into<String>, id: u64) -> Self {
        Self {
            socket: socket.into(),
            id,
        }
    }

    /// Short display form for logs, e.g. `42@12345`.
    pub fn short(&self) -> String {
        let sock_short = self
            .socket
            .rsplit("kitty.sock-")
            .next()
            .unwrap_or(&self.socket);
        format!("{}@{}", self.id, sock_short)
    }
}

impl fmt::Display for PaneAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.socket, self.id)
    }
}

impl FromStr for PaneAddr {
    type Err = ParsePaneAddrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (socket, id) = value
            .rsplit_once(':')
            .ok_or(ParsePaneAddrError::MissingSeparator)?;
        if socket.is_empty() {
            return Err(ParsePaneAddrError::MissingSocket);
        }
        let id = id
            .parse::<u64>()
            .map_err(|_| ParsePaneAddrError::InvalidId)?;
        Ok(Self::new(socket, id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsePaneAddrError {
    MissingSeparator,
    MissingSocket,
    InvalidId,
}

impl fmt::Display for ParsePaneAddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSeparator => write!(f, "pane address must be '<socket>:<id>'"),
            Self::MissingSocket => write!(f, "pane address socket is empty"),
            Self::InvalidId => write!(f, "pane address id must be an unsigned integer"),
        }
    }
}

impl std::error::Error for ParsePaneAddrError {}

/// Harness-native session id.
///
/// This preserves the provider's own stable id without assuming it is globally
/// unique across every harness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Globally unambiguous session key when multiple harnesses share id formats.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionKey {
    pub agent_kind: AgentKind,
    pub session_id: SessionId,
}

impl AgentSessionKey {
    pub fn new(agent_kind: AgentKind, session_id: impl Into<SessionId>) -> Self {
        Self {
            agent_kind,
            session_id: session_id.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        self.agent_kind.session_key(self.session_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_addr_roundtrips_display_parse_and_json() {
        let addr = PaneAddr::new("unix:/run/user/1000/kitty.sock-12345", 42);

        assert_eq!(addr.short(), "42@12345");
        assert_eq!(addr.to_string(), "unix:/run/user/1000/kitty.sock-12345:42");
        assert_eq!(addr.to_string().parse::<PaneAddr>().unwrap(), addr);
        assert_eq!(
            serde_json::to_value(&addr).unwrap(),
            json!({
                "socket": "unix:/run/user/1000/kitty.sock-12345",
                "id": 42
            })
        );
    }

    #[test]
    fn pane_addr_distinguishes_same_id_on_different_sockets() {
        let one = PaneAddr::new("unix:/run/user/1000/kitty.sock-111", 7);
        let two = PaneAddr::new("unix:/run/user/1000/kitty.sock-222", 7);

        assert_ne!(one, two);
    }

    #[test]
    fn session_key_namespaces_provider_native_ids() {
        let claude = AgentSessionKey::new(AgentKind::Claude, "shared-id");
        let codex = AgentSessionKey::new(AgentKind::Codex, "shared-id");

        assert_eq!(claude.session_id.as_str(), "shared-id");
        assert_ne!(claude, codex);
        assert_eq!(claude.storage_key(), "claude:shared-id");
        assert_eq!(codex.storage_key(), "codex:shared-id");
    }

    #[test]
    fn session_id_is_transparent_json() {
        assert_eq!(
            serde_json::to_value(SessionId::new("sess-1")).unwrap(),
            json!("sess-1")
        );
    }
}
